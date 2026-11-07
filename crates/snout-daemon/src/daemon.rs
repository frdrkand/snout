//! The check schedule, the escalation, and the backoff.
//!
//! Its existence is what makes checks independent of any user interface: a
//! polling status bar drives nothing, so a `status` call is always cheap and
//! always safe, and checks still happen on a machine where the bar was never
//! started.
//!
//! It never interrupts. snout raises no desktop notifications and has no way
//! to: the answer lives in the cache, and it is read when somebody chooses to
//! look. A tool that tells you about updates you did not ask about is a tool
//! you end up muting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use snout_common::config::Config;
use snout_common::docs::{Phase, Status};
use snout_common::fail::{Error, Result};
use snout_common::run::Runner;
use snout_common::time::{Timestamp, clock, now as clock_now};

use crate::check::{Checker, Clock};
use crate::log::Logger;
use crate::server;
use crate::store::Store;

pub struct Daemon {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub log: Logger,
    pub now: Clock,
    runner: Runner,
    /// Anchors the settle delay, which keeps the first check away from login —
    /// the worst possible moment to start fetching.
    started_at: Timestamp,
    /// True while a check is running, so a status call can say why the answer
    /// it just gave might be different in a minute.
    busy: AtomicBool,
    /// Set by [`Daemon::invalidate`] and [`Daemon::request_check`]: check on
    /// the next pass whatever the schedule says.
    forced: AtomicBool,
    /// Wakes the tick loop early, so an invalidation is acted on in
    /// milliseconds rather than whenever the schedule next comes round.
    wake: Notify,
    /// When the running check started and the step it is on. Shared with the
    /// checker, which is the only thing that knows when a step changes.
    progress: Arc<Mutex<Running>>,
}

/// What `status` reports about a check in flight. Both empty when idle.
#[derive(Debug, Default, Clone, Copy)]
struct Running {
    started_at: Option<Timestamp>,
    phase: Option<Phase>,
}

impl Daemon {
    /// Builds a daemon that is immediately usable.
    ///
    /// Everything is constructed here rather than when `run` starts, so there
    /// is no window in which a status call can reach a half-built daemon.
    pub fn new(cfg: Config, runner: Runner) -> Result<Daemon> {
        let store = Arc::new(Store::new(cfg.updates_dir()?));
        let log = Logger::new(crate::log::Sink::Stderr, &cfg.log_level);
        Ok(Self::with_parts(
            cfg,
            runner,
            store,
            log,
            Arc::new(clock_now),
        ))
    }

    pub fn with_parts(
        cfg: Config,
        runner: Runner,
        store: Arc<Store>,
        log: Logger,
        now: Clock,
    ) -> Daemon {
        Daemon {
            started_at: now(),
            cfg,
            store,
            log,
            now,
            runner,
            busy: AtomicBool::new(false),
            forced: AtomicBool::new(false),
            wake: Notify::new(),
            progress: Arc::new(Mutex::new(Running::default())),
        }
    }

    fn now(&self) -> Timestamp {
        (self.now)()
    }

    /// The running system's store path, or `None` if it cannot be resolved.
    ///
    /// Every answer the daemon serves is filtered through this: a cached
    /// document stamped with another generation describes a system that has
    /// been switched away from, and that is not stale, it is wrong.
    pub fn current_system(&self) -> Option<String> {
        (self.store.current_system)().ok()
    }

    /// A checker for one run.
    ///
    /// A fresh one each time rather than a stored one, so a run cannot inherit
    /// anything from the previous one. Its store must resolve the running
    /// system the same way ours does: if the check measured against one notion
    /// of the running system and the publish guard compared against another,
    /// every answer would look stale.
    fn checker(&self) -> Checker {
        let mut store = Store::new(self.store.dir.clone());
        store.current_system = Arc::clone(&self.store.current_system);
        let mut c = Checker::new(
            self.cfg.clone(),
            self.runner.clone(),
            store,
            self.log.clone(),
        );
        c.now = Arc::clone(&self.now);
        let progress = Arc::clone(&self.progress);
        c.progress = Arc::new(move |p| {
            if let Ok(mut r) = progress.lock() {
                r.phase = Some(p);
            }
        });
        c
    }

    /// Serves the control socket and ticks until cancelled.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        // Refusing here as well as in the module means the setting says the
        // same thing however the daemon was launched.
        if !self.cfg.daemon.enable {
            return Err(Error::config(
                "the daemon is disabled — set daemon.enable to run it",
            ));
        }

        self.store.ensure()?;
        let listener = server::listen()?;

        self.log.info(format!(
            "watching {}#{}, checking every {}",
            self.cfg.flake, self.cfg.host, self.cfg.daemon.interval
        ));

        let serving = tokio::spawn(server::serve(Arc::clone(&self), listener, cancel.clone()));

        self.tick_loop(&cancel).await;

        cancel.cancel();
        // Waited on rather than abandoned: a handler mid-write must finish, or
        // a client is left reading a truncated line.
        let _ = serving.await;
        Ok(())
    }

    /// Sleeps until the next check is due, checks, and repeats.
    ///
    /// One consequence of sleeping rather than polling a staleness comparison
    /// on a short timer is worth knowing: the wait is a monotonic timer, and
    /// the schedule it was computed from is wall-clock. A suspend does not
    /// advance the timer, so a laptop that slept through the interval resumes
    /// with the remainder of its sleep still to run, and checks that much later
    /// than the wall clock says it was due.
    async fn tick_loop(&self, cancel: &CancellationToken) {
        loop {
            // Recomputed every pass, so a check that ran long, a backoff, or an
            // invalidation all move the next wake-up with them.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(self.until_next_check()) => {}
                _ = self.wake.notified() => {}
            }
            // An invalidation is consumed whether or not the schedule agrees:
            // the caller has told us the cache describes a world that no longer
            // exists, and the settle delay is about login, not about that.
            //
            // `busy` goes up before `forced` comes down. `status` reports busy
            // as either, so this order leaves no instant in which a requested
            // check looks finished before it has started — which is what
            // `snout check` waits on.
            if self.forced.load(Ordering::SeqCst) || self.due() {
                self.busy.store(true, Ordering::SeqCst);
                self.forced.store(false, Ordering::SeqCst);
                self.check(cancel).await;
                self.busy.store(false, Ordering::SeqCst);
            }
        }
    }

    /// How long until the schedule fires, and zero when a check is already
    /// owed.
    ///
    /// [`Daemon::next_check_at`] and [`Daemon::due`] read the same three
    /// things — the settle delay, the last check, the backoff — so a wait of
    /// zero always means `due` is about to agree, and the loop cannot spin.
    fn until_next_check(&self) -> StdDuration {
        self.next_check_at()
            .signed_duration_since(self.now())
            .to_std()
            .unwrap_or(StdDuration::ZERO)
    }

    /// Runs one check and records what it did.
    async fn check(&self, cancel: &CancellationToken) {
        self.set_running(Running {
            started_at: Some(self.now()),
            phase: None,
        });
        let outcome = self.checker().run(cancel).await;
        self.set_running(Running::default());
        self.record(outcome.as_ref().err());
    }

    fn set_running(&self, r: Running) {
        if let Ok(mut cur) = self.progress.lock() {
            *cur = r;
        }
    }

    fn running(&self) -> Running {
        self.progress.lock().map(|r| *r).unwrap_or_default()
    }

    /// Brings the scheduled check forward, keeping the cache.
    ///
    /// Returns false when a check is already running: that one is the answer
    /// the caller is after, and queueing a second behind it would only repeat
    /// it. Otherwise this forgets the lock that last failed to build — asking
    /// by hand is a request to try again, and a check that then skipped the
    /// build would not be the one that was asked for. The backoff needs no
    /// such help: a forced pass does not consult it.
    pub fn request_check(&self) -> bool {
        if self.busy.load(Ordering::SeqCst) {
            return false;
        }
        self.store.clear_failed_lock();
        self.forced.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        self.log.info("check requested");
        true
    }

    /// Drops the cached answers and checks again at once.
    ///
    /// Called when the CLI has deployed or rewritten the lock. Both leave the
    /// cache holding statements about a system that has this second stopped
    /// being the one running, and waiting out a whole interval would advertise
    /// an update that is already installed.
    ///
    /// The clear happens here, synchronously, so a `status` taken immediately
    /// afterwards is already honest; the check that replaces it is left to the
    /// tick loop, which this wakes.
    pub fn invalidate(&self) -> Result<()> {
        self.store.clear()?;
        self.forced.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        self.log.info("cache cleared, checking again");
        Ok(())
    }

    /// Whether a check is owed right now.
    pub fn due(&self) -> bool {
        let now = self.now();
        let st = self.store.read_state();

        if let Some(until) = st.backoff_until
            && now < until
        {
            return false;
        }
        // The settle delay is a floor on the first check, not a timer of its
        // own.
        if now < self.started_at + self.cfg.daemon.settle_delay.as_std() {
            return false;
        }

        match self.last_checked_at() {
            None => true,
            Some(last) => now
                .signed_duration_since(last)
                .to_std()
                .is_ok_and(|d| d >= self.cfg.daemon.interval.as_std()),
        }
    }

    /// Prefers the published answer over the daemon's own bookkeeping, so an
    /// answer that landed some other way also resets the schedule — there is no
    /// point re-asking a question that was answered a minute ago.
    fn last_checked_at(&self) -> Option<Timestamp> {
        match self.store.read_quick() {
            Some(q) => Some(q.checked_at),
            None => self.store.read_state().last_checked_at,
        }
    }

    /// When the schedule will next fire, for the status snapshot.
    pub fn next_check_at(&self) -> Timestamp {
        let mut next = self.started_at + self.cfg.daemon.settle_delay.as_std();

        if let Some(last) = self.last_checked_at() {
            let t = last + self.cfg.daemon.interval.as_std();
            if t > next {
                next = t;
            }
        }
        if let Some(b) = self.store.read_state().backoff_until
            && b > next
        {
            next = b;
        }
        next
    }

    /// Updates the private bookkeeping: when the last check succeeded, how many
    /// have failed in a row, and what the last failure said.
    pub fn record(&self, err: Option<&Error>) {
        let mut st = self.store.read_state();
        let now = self.now();

        match err {
            Some(e) => {
                st.failures += 1;
                let wait = self.backoff_for(st.failures);
                st.backoff_until = Some(now + wait);
                // Kept so `status` can say that checking is broken. Neither
                // cached answer can: a failed check never overwrites a good
                // one, so both go on describing the last time this worked.
                st.last_error = Some(e.to_string());
                st.last_error_code = Some(e.code);
                self.log.warn(format!(
                    "check failed ({} in a row, next attempt {}): {e}",
                    st.failures,
                    clock(now + wait)
                ));
            }
            None => {
                st.failures = 0;
                st.backoff_until = None;
                st.last_error = None;
                st.last_error_code = None;
                st.last_checked_at = Some(now);
            }
        }

        if let Err(e) = self.store.write_state(&st) {
            self.log.error(format!("writing state: {e}"));
        }
    }

    /// Doubles from the configured minimum up to the maximum.
    ///
    /// This is for transient failures — a network blip should be retried in
    /// five minutes, not in two hours. A *build* that fails is handled
    /// separately and keyed on the lock it failed for, since that one repeats
    /// identically until upstream moves.
    fn backoff_for(&self, failures: u32) -> StdDuration {
        let min = self.cfg.daemon.backoff.min.as_std();
        let max = self.cfg.daemon.backoff.max.as_std();
        let mut wait = min;
        for _ in 1..failures.max(1) {
            if wait >= max {
                break;
            }
            wait *= 2;
        }
        wait.min(max)
    }

    /// Assembles the snapshot, adding what only the daemon knows.
    pub fn status(&self) -> Status {
        let current = (self.store.current_system)().ok();
        let mut st = self
            .store
            .snapshot(&self.cfg.host, &self.cfg.flake, current.as_deref());

        st.daemon.running = true;
        // An owed check counts: between a request and the loop picking it up,
        // "not busy" would tell a waiting `snout check` it had already finished.
        st.daemon.busy = self.busy.load(Ordering::SeqCst) || self.forced.load(Ordering::SeqCst);
        st.daemon.next_check_at = Some(self.next_check_at());
        st.daemon.backoff_until = self.store.read_state().backoff_until;
        let r = self.running();
        st.daemon.phase = r.phase;
        st.daemon.started_at = r.started_at;
        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log;
    use crate::store::State;
    use chrono::TimeDelta;
    use snout_common::config::Duration;
    use snout_common::docs::Quick;
    use snout_common::fail::Code;
    use std::sync::Mutex;

    const RUNNING: &str = "/nix/store/aaa-system";

    struct Harness {
        _dir: tempfile::TempDir,
        daemon: Arc<Daemon>,
        clock: Arc<Mutex<Timestamp>>,
        capture: log::Capture,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            flake: "/etc/nixos".into(),
            host: "somehost".into(),
            cache_dir: dir.path().join("cache").to_string_lossy().into_owned(),
            ..Config::default()
        };
        let mut store = Store::new(cfg.updates_dir().unwrap());
        store.current_system = Arc::new(|| Ok(RUNNING.into()));
        store.ensure().unwrap();

        let clock = Arc::new(Mutex::new(clock_now()));
        let c = Arc::clone(&clock);
        let (logger, capture) = log::Capture::logger("debug");

        let daemon = Arc::new(Daemon::with_parts(
            cfg,
            Runner::fake(),
            Arc::new(store),
            logger,
            Arc::new(move || *c.lock().unwrap()),
        ));
        Harness {
            _dir: dir,
            daemon,
            clock,
            capture,
        }
    }

    impl Harness {
        fn advance(&self, d: TimeDelta) {
            let mut c = self.clock.lock().unwrap();
            *c += d;
        }
    }

    fn quick_at(at: Timestamp) -> Quick {
        Quick {
            schema_version: 1,
            checked_at: at,
            available: false,
            moved_count: 0,
            moved: vec![],
            lock_hash: "x".into(),
            current_system: RUNNING.into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
        }
    }

    // -- due() -------------------------------------------------------------

    #[test]
    fn nothing_is_due_during_the_settle_delay() {
        // Login is the worst moment to start fetching, and the answer on disk
        // is usually still good.
        let h = harness();
        assert!(!h.daemon.due());
        h.advance(TimeDelta::minutes(4));
        assert!(!h.daemon.due());
    }

    #[test]
    fn the_first_check_is_due_once_the_settle_delay_passes() {
        let h = harness();
        h.advance(TimeDelta::minutes(6));
        assert!(h.daemon.due());
    }

    #[test]
    fn nothing_is_due_again_until_the_interval_has_elapsed() {
        // The interval is an hour, so the boundary is at 60 minutes.
        let h = harness();
        h.advance(TimeDelta::minutes(6));
        h.daemon.record(None);

        assert!(!h.daemon.due());
        h.advance(TimeDelta::minutes(59));
        assert!(!h.daemon.due());
        h.advance(TimeDelta::minutes(2));
        assert!(h.daemon.due());
    }

    #[test]
    fn a_published_answer_resets_the_schedule() {
        // There is no point re-asking a question that was answered a minute
        // ago, whichever code path answered it.
        let h = harness();
        h.advance(TimeDelta::minutes(6));
        assert!(h.daemon.due());

        let q = quick_at(h.daemon.now());
        h.daemon
            .store
            .publish(&h.daemon.store.quick_path(), &q, "")
            .unwrap();
        assert!(!h.daemon.due());
    }

    #[test]
    fn nothing_is_due_while_a_backoff_is_running() {
        let h = harness();
        h.advance(TimeDelta::hours(3));
        h.daemon
            .store
            .write_state(&State {
                backoff_until: Some(h.daemon.now() + TimeDelta::minutes(10)),
                ..Default::default()
            })
            .unwrap();
        assert!(!h.daemon.due());
        h.advance(TimeDelta::minutes(11));
        assert!(h.daemon.due());
    }

    // -- invalidate --------------------------------------------------------

    #[test]
    fn invalidate_empties_the_cache_at_once_and_arms_the_next_pass() {
        // Synchronous clear: a `status` taken immediately after a deploy must
        // not still report the update that deploy just installed.
        let h = harness();
        h.daemon
            .store
            .publish(&h.daemon.store.quick_path(), &quick_at(h.daemon.now()), "")
            .unwrap();
        h.daemon.store.set_failed_lock("abc");

        h.daemon.invalidate().unwrap();

        assert!(h.daemon.store.read_quick().is_none());
        assert_eq!(h.daemon.store.failed_lock(), "");
        assert!(h.daemon.forced.load(Ordering::SeqCst));
    }

    #[test]
    fn an_invalidation_checks_even_inside_the_settle_delay() {
        // The settle delay is about login, not about a deploy that just landed.
        let h = harness();
        assert!(!h.daemon.due());
        h.daemon.invalidate().unwrap();
        assert!(
            h.daemon.forced.swap(false, Ordering::SeqCst),
            "the invalidation did not arm a check"
        );
    }

    // -- backoff -----------------------------------------------------------

    #[test]
    fn backoff_doubles_from_the_minimum_and_stops_at_the_maximum() {
        let h = harness();
        let mins = |d: StdDuration| d.as_secs() / 60;
        assert_eq!(mins(h.daemon.backoff_for(1)), 5);
        assert_eq!(mins(h.daemon.backoff_for(2)), 10);
        assert_eq!(mins(h.daemon.backoff_for(3)), 20);
        assert_eq!(mins(h.daemon.backoff_for(4)), 40);
        assert_eq!(mins(h.daemon.backoff_for(5)), 80);
        assert_eq!(mins(h.daemon.backoff_for(6)), 120);
        // Capped, however many failures accumulate.
        assert_eq!(mins(h.daemon.backoff_for(50)), 120);
        // A zero count is clamped rather than producing no wait at all.
        assert_eq!(mins(h.daemon.backoff_for(0)), 5);
    }

    #[test]
    fn a_failure_records_the_error_its_code_and_the_next_attempt() {
        let h = harness();
        h.daemon
            .record(Some(&Error::network("updating flake inputs: exit 1")));

        let st = h.daemon.store.read_state();
        assert_eq!(st.failures, 1);
        assert_eq!(st.last_error_code, Some(Code::Network));
        assert!(st.last_error.unwrap().contains("updating flake inputs"));
        assert!(st.backoff_until.is_some());
        assert!(st.last_checked_at.is_none(), "a failure is not a check");

        let lines = h.capture.lines();
        assert!(
            lines.iter().any(|l| l.contains("check failed (1 in a row")),
            "{lines:?}"
        );
    }

    #[test]
    fn consecutive_failures_accumulate_and_a_success_clears_everything() {
        let h = harness();
        let e = Error::network("boom");
        h.daemon.record(Some(&e));
        h.daemon.record(Some(&e));
        assert_eq!(h.daemon.store.read_state().failures, 2);

        h.daemon.record(None);
        let st = h.daemon.store.read_state();
        assert_eq!(
            st,
            State {
                last_checked_at: st.last_checked_at,
                ..Default::default()
            }
        );
        assert!(st.last_checked_at.is_some());
    }

    // -- next_check_at -----------------------------------------------------

    #[test]
    fn the_next_check_is_the_latest_of_settle_interval_and_backoff() {
        let h = harness();
        let start = h.daemon.now();

        // Nothing checked yet: the settle delay is the whole answer.
        assert_eq!(h.daemon.next_check_at(), start + TimeDelta::minutes(5));

        h.advance(TimeDelta::minutes(6));
        h.daemon.record(None);
        assert_eq!(
            h.daemon.next_check_at(),
            start + TimeDelta::minutes(6) + TimeDelta::hours(1)
        );

        // A backoff further out wins.
        let far = h.daemon.now() + TimeDelta::hours(5);
        h.daemon
            .store
            .write_state(&State {
                backoff_until: Some(far),
                last_checked_at: Some(h.daemon.now()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(h.daemon.next_check_at(), far);
    }

    #[test]
    fn an_owed_check_waits_for_nothing_and_does_not_spin() {
        // A wait of zero has to mean `due` agrees, or the loop would sleep for
        // no time and decide to do nothing, over and over.
        let h = harness();
        h.advance(TimeDelta::hours(9));
        assert_eq!(h.daemon.until_next_check(), StdDuration::ZERO);
        assert!(h.daemon.due());

        h.daemon.record(None);
        h.advance(TimeDelta::hours(2));
        assert_eq!(h.daemon.until_next_check(), StdDuration::ZERO);
        assert!(h.daemon.due());
    }

    // -- status ------------------------------------------------------------

    #[test]
    fn status_reports_the_daemon_as_running_with_its_next_check() {
        let h = harness();
        let st = h.daemon.status();
        assert!(st.daemon.running);
        assert!(st.daemon.next_check_at.is_some());
        assert!(!st.daemon.busy);
    }

    #[test]
    fn an_idle_status_has_no_phase_and_no_start_time() {
        let h = harness();
        let st = h.daemon.status();
        assert!(st.daemon.phase.is_none());
        assert!(st.daemon.started_at.is_none());
    }

    #[test]
    fn a_running_check_reports_its_phase_and_when_it_started() {
        let h = harness();
        let at = h.daemon.now();
        h.daemon.set_running(Running {
            started_at: Some(at),
            phase: None,
        });
        // The checker the daemon builds is wired to the same progress cell.
        (h.daemon.checker().progress)(Phase::Building);

        let st = h.daemon.status();
        assert_eq!(st.daemon.phase, Some(Phase::Building));
        assert_eq!(st.daemon.started_at, Some(at));
    }

    // -- request_check -----------------------------------------------------

    #[test]
    fn a_requested_check_is_armed_keeps_the_cache_and_forgets_a_failed_build() {
        let h = harness();
        h.daemon
            .store
            .publish(&h.daemon.store.quick_path(), &quick_at(h.daemon.now()), "")
            .unwrap();
        h.daemon.store.set_failed_lock("abc");

        assert!(h.daemon.request_check());

        assert!(h.daemon.forced.load(Ordering::SeqCst));
        // Unlike invalidate, the answer on disk is still true until replaced.
        assert!(h.daemon.store.read_quick().is_some());
        assert_eq!(h.daemon.store.failed_lock(), "");
    }

    #[test]
    fn an_owed_check_already_reads_as_busy() {
        // `snout check` waits for busy to clear. If the request alone did not
        // set it, the first status after asking would say "done".
        let h = harness();
        assert!(!h.daemon.status().daemon.busy);
        h.daemon.request_check();
        assert!(h.daemon.status().daemon.busy);
    }

    #[test]
    fn a_check_requested_while_one_runs_attaches_to_it() {
        let h = harness();
        h.daemon.busy.store(true, Ordering::SeqCst);
        h.daemon.store.set_failed_lock("abc");

        assert!(!h.daemon.request_check());
        assert!(
            !h.daemon.forced.load(Ordering::SeqCst),
            "queued a second check"
        );
        assert_eq!(h.daemon.store.failed_lock(), "abc");
    }

    #[test]
    fn status_surfaces_a_running_backoff() {
        let h = harness();
        h.daemon.record(Some(&Error::build("nope")));
        let st = h.daemon.status();
        assert!(st.daemon.backoff_until.is_some());
        assert_eq!(st.error.unwrap().code, Code::Build);
    }

    #[test]
    fn a_disabled_daemon_refuses_to_start() {
        // The setting means the same thing however the daemon was launched.
        let h = harness();
        let mut cfg = h.daemon.cfg.clone();
        cfg.daemon.enable = false;
        let d = Arc::new(Daemon::with_parts(
            cfg,
            Runner::fake(),
            Arc::clone(&h.daemon.store),
            Logger::discard(),
            Arc::clone(&h.daemon.now),
        ));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(d.run(CancellationToken::new())).unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(err.to_string().contains("daemon is disabled"), "{err}");
    }

    #[test]
    fn a_settle_delay_of_zero_is_rejected_before_it_can_fire_at_login() {
        let mut cfg = Config {
            flake: "/etc/nixos".into(),
            host: "somehost".into(),
            ..Config::default()
        };
        cfg.daemon.settle_delay = Duration::from_secs(0);
        assert!(cfg.validate().is_err());
    }
}
