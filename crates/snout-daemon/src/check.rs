//! One check: has anything moved, and if so, what would actually change.
//!
//! ```text
//! stage 1   Copy the flake to a scratch directory, run `nix flake update` on
//!           the copy, compare the two locks. Network only, seconds. Answers
//!           "upstream has moved past what is installed".
//!
//! stage 2   Build the updated system and diff its closure against the running
//!           one. Minutes, and gigabytes when nixpkgs has moved. Answers "which
//!           packages would change" — so a count of 87 means 87 packages, not 4
//!           inputs.
//! ```
//!
//! Stage 2 runs only when stage 1 says something moved *and* the package diff
//! already on disk has stopped describing the current lock. So the expensive
//! answer is computed at the moment the cheap one first knows it is needed —
//! hours before anyone asks to read it, which is the whole point: the
//! alternative is starting a ten-minute build when someone types
//! `snout list-updates`.
//!
//! Nothing here takes a "which stage" parameter. The daemon is the only caller,
//! it always wants the cheapest correct answer, and a knob nobody turns is a
//! branch nobody tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use snout_common::config::Config;
use snout_common::docs::{CURRENT_SYSTEM_LINK, Full, Phase, Quick, SCHEMA_VERSION};
use snout_common::fail::{Code, Context, Error, Result};
use snout_common::lockdiff;
use snout_common::nix;
use snout_common::run::{Cmd, Runner};
use snout_common::time::{Timestamp, now as clock_now};

use crate::log::Logger;
use crate::store::{Store, SystemProbe};

/// A clock, injectable so the whole decision tree is testable without waiting.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// Told which step a check has reached, so `status` can say more than "busy".
pub type Progress = Arc<dyn Fn(Phase) + Send + Sync>;

/// Runs a check and publishes what it found.
pub struct Checker {
    pub cfg: Config,
    pub runner: Runner,
    pub store: Store,
    pub now: Clock,
    pub current_system: SystemProbe,
    pub log: Logger,
    pub progress: Progress,
}

impl Checker {
    pub fn new(cfg: Config, runner: Runner, store: Store, log: Logger) -> Checker {
        let current_system = Arc::clone(&store.current_system);
        Checker {
            cfg,
            runner,
            store,
            now: Arc::new(clock_now),
            current_system,
            log,
            progress: Arc::new(|_| {}),
        }
    }

    fn now(&self) -> Timestamp {
        (self.now)()
    }

    fn phase(&self, p: Phase) {
        (self.progress)(p);
    }

    fn probe_system(&self) -> Result<String> {
        (self.current_system)().map_err(|e| {
            Error::wrap(
                Code::Precondition,
                e,
                format!("reading {CURRENT_SYSTEM_LINK}"),
            )
        })
    }

    /// Performs a check, leaving its answers in the cache.
    ///
    /// The return value is deliberately empty: the cache *is* the output, and a
    /// caller that read a result here instead of the cache would be reading a
    /// different thing from every other reader.
    pub async fn run(&self, cancel: &CancellationToken) -> Result<()> {
        self.store.ensure()?;

        let _lock = self.store.acquire(|| {
            self.phase(Phase::Waiting);
            self.log.debug("waiting for another check to finish");
        })?;

        // Read before any work starts: it is what stage 2 diffs against, what
        // both caches are stamped with, and the marker `publish` compares.
        let current = self.probe_system()?;

        let (quick, lock_hash) = self.run_quick(cancel, &current).await?;

        // The whole decision, in the order that makes each case cheapest:
        //
        //   the diff is still current   reuse it — an answer that cannot have
        //                               changed is not a reason to build
        //   nothing moved, or this
        //   lock already failed to
        //   build                       stop at the cheap answer
        //   otherwise                   build
        if let Some(full) = self.current_full(&lock_hash, &current) {
            let mut restamped = full;
            restamped.checked_at = self.now();
            self.store
                .publish(&self.store.full_path(), &restamped, &current)?;
            // Reached without building, but the lock plainly does build: the
            // answer being reused was produced by a successful one. Leaving the
            // marker would suppress escalation for this lock forever.
            self.store.clear_failed_lock();
            self.log.info(format!(
                "{} to update (unchanged since the last check)",
                restamped.summary.count
            ));
            return Ok(());
        }

        if !self.escalate_wanted(&quick, &lock_hash) {
            return Ok(());
        }

        match self.run_full(&current, &lock_hash).await {
            Ok(full) => {
                self.store.clear_failed_lock();
                self.log
                    .info(format!("{} packages would change", full.summary.count));
                Ok(())
            }
            Err(e) => {
                // The lock this failed on is recorded so the automatic
                // escalation skips it: a broken nixpkgs or a full disk fails
                // the same way every hour, and re-downloading gigabytes to
                // reach the same error is not diligence.
                //
                // Cancellation is excluded. A daemon shutting down mid-build is
                // not a verdict on the lock, and recording one would stop
                // escalation from ever retrying it.
                if !cancel.is_cancelled() {
                    self.store.set_failed_lock(&lock_hash);
                }
                Err(e)
            }
        }
    }

    /// Copies the flake, updates the copy's lock, and compares the two.
    async fn run_quick(
        &self,
        _cancel: &CancellationToken,
        current: &str,
    ) -> Result<(Quick, String)> {
        self.phase(Phase::Copying);
        self.log.debug("copying the flake");
        self.copy_flake().await?;

        let work = self.store.work_path();
        let lock_path = work.join("flake.lock");

        let before =
            std::fs::read(&lock_path).context(Code::Config, "the flake has no flake.lock")?;

        self.phase(Phase::Updating);
        self.log.debug("updating inputs");
        let cmd = Cmd::new(["nix", "flake", "update", "--flake", &work.to_string_lossy()]);
        let mut filter = nix::LogFilter::default();
        let (_, res) = self
            .runner
            .run_streaming(&cmd, Some(|line: &str| self.log_line(&mut filter, line)))
            .await;
        self.flush_log(&mut filter);
        res.context(Code::Network, "updating flake inputs")?;

        let after =
            std::fs::read(&lock_path).context(Code::Internal, "reading the updated lock")?;

        let moved = lockdiff::diff(&before, &after)
            .map_err(|e| Error::wrap(Code::Internal, e, "comparing locks"))?;

        let lock_hash = hex::encode(Sha256::digest(&after));

        self.log.info(if moved.is_empty() {
            "inputs are up to date".to_string()
        } else {
            format!("{} inputs moved", moved.len())
        });

        let q = Quick {
            schema_version: SCHEMA_VERSION,
            checked_at: self.now(),
            available: !moved.is_empty(),
            moved_count: moved.len(),
            moved,
            lock_hash: lock_hash.clone(),
            current_system: current.to_string(),
            host: self.cfg.host.clone(),
            flake: self.cfg.flake.clone(),
        };
        self.store.publish(&self.store.quick_path(), &q, current)?;
        Ok((q, lock_hash))
    }

    /// Builds the updated system and diffs its closure against the running one.
    async fn run_full(&self, current: &str, lock_hash: &str) -> Result<Full> {
        let work = self.store.work_path();
        let attr = self.cfg.toplevel_attr(&work.to_string_lossy());

        self.phase(Phase::Building);
        self.log.info("building the updated system");
        let cmd = Cmd::new([
            "nix",
            "build",
            &attr,
            "--out-link",
            &self.store.result_link().to_string_lossy(),
        ]);
        let mut filter = nix::LogFilter::default();
        let (_, res) = self
            .runner
            .run_streaming(&cmd, Some(|line: &str| self.log_line(&mut filter, line)))
            .await;
        self.flush_log(&mut filter);
        res.context(Code::Build, "building the updated system")?;

        let built = std::fs::canonicalize(self.store.result_link())
            .context(Code::Internal, "resolving the build result")?;

        self.phase(Phase::Comparing);
        self.log.debug("comparing closures");
        let diff = nix::diff_paths(&self.runner, current, &built.to_string_lossy())
            .await
            .map_err(|e| Error::wrap(Code::Internal, e, "comparing closures"))?;

        let f = Full {
            schema_version: SCHEMA_VERSION,
            checked_at: self.now(),
            lock_hash: lock_hash.to_string(),
            current_system: current.to_string(),
            host: self.cfg.host.clone(),
            flake: self.cfg.flake.clone(),
            summary: diff.summary(),
            updates: diff.changes,
        };
        self.store.publish(&self.store.full_path(), &f, current)?;
        Ok(f)
    }

    /// Forwards one line of nix's stderr to the journal, minus the parts nobody
    /// can act on.
    ///
    /// At debug level: on a cold nixpkgs this is thousands of lines, and a
    /// journal that fills with build output is one nobody reads the warnings
    /// in. The filter is per command rather than per checker — it carries a
    /// position within one stream, and a leftover body from the previous
    /// command would swallow the start of the next.
    fn log_line(&self, filter: &mut nix::LogFilter, line: &str) {
        for line in filter.push(line) {
            self.log.debug(&line);
        }
    }

    /// Reports a body that ran to the end of the stream.
    fn flush_log(&self, filter: &mut nix::LogFilter) {
        for line in filter.finish() {
            self.log.debug(&line);
        }
    }

    /// The cached package diff if it still describes this lock on this system.
    /// A hit is both a reason to reuse the cached answer and a reason not to
    /// build it again — one test, two uses.
    fn current_full(&self, lock_hash: &str, current: &str) -> Option<Full> {
        let f = self.store.read_full()?;
        (f.lock_hash == lock_hash && f.current_system == current).then_some(f)
    }

    /// Only asked when the cached diff is *not* current: upstream has moved
    /// past what the list describes, so the list is stale the second the
    /// indicator lights up — and it takes minutes to rebuild, which is
    /// precisely why it should not wait for someone to ask.
    fn escalate_wanted(&self, q: &Quick, lock_hash: &str) -> bool {
        q.available && self.store.failed_lock() != lock_hash
    }

    /// Reproduces nix's own source semantics for a dirty git flake: the
    /// working-tree contents of tracked files, untracked files excluded.
    ///
    /// `nix flake update` rewrites `flake.lock` in place, and doing that to the
    /// user's working tree behind their back is not acceptable — a check must
    /// never be something you have to `git checkout` afterwards. Hence a copy;
    /// and it is `git ls-files` rather than a recursive copy because the latter
    /// drags in `.git`, which on a real configuration repo is two orders of
    /// magnitude larger than the flake itself.
    async fn copy_flake(&self) -> Result<()> {
        let work = self.store.work_path();
        std::fs::remove_dir_all(&work)
            .or_else(ignore_missing)
            .context(Code::Internal, "clearing the scratch copy")?;
        std::fs::create_dir_all(&work).context(Code::Internal, "creating the scratch copy")?;

        let cmd = Cmd::new(["git", "-C", &self.cfg.flake, "ls-files", "-z"]);
        let (out, res) = self.runner.run(&cmd).await;
        res.with_context(Code::Config, || {
            format!("listing tracked files in {}", self.cfg.flake)
        })?;

        let listing = String::from_utf8_lossy(&out.stdout);
        let mut copied = 0usize;
        for name in listing.trim_end_matches('\0').split('\0') {
            if name.is_empty() {
                continue;
            }
            // Counts what actually landed, not what git listed. A tracked file
            // deleted from the working tree is skipped, and counting it anyway
            // lets a fully-deleted tree slip past the guard below and fail
            // later with a message about a missing flake.lock.
            if copy_one(Path::new(&self.cfg.flake), &work, name)
                .with_context(Code::Internal, || format!("copying {name}"))?
            {
                copied += 1;
            }
        }
        if copied == 0 {
            return Err(Error::config(format!(
                "{} has no tracked files — is it a git work tree?",
                self.cfg.flake
            )));
        }
        Ok(())
    }
}

fn ignore_missing(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(e)
    }
}

/// Copies a single tracked entry, preserving the file mode, and reports whether
/// anything was copied. The mode matters: nix hashes it into the source, so
/// dropping the executable bit on a script changes what gets built.
fn copy_one(src_root: &Path, dst_root: &Path, name: &str) -> std::io::Result<bool> {
    let src = src_root.join(name);
    let dst: PathBuf = dst_root.join(name);

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let meta = match std::fs::symlink_metadata(&src) {
        Ok(m) => m,
        // A tracked file that is not in the working tree — deleted but not yet
        // committed — is simply not copied, which is what nix would see too.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&src)?;
        std::os::unix::fs::symlink(target, &dst)?;
        return Ok(true);
    }

    std::fs::copy(&src, &dst)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use snout_common::run::Output;

    const RUNNING: &str = "/nix/store/aaa-system";

    /// A scratch flake, a fake runner, and a checker wired to both.
    ///
    /// `updated` is the single toggle that drives the whole decision: it
    /// decides whether `nix flake update` moves the scratch lock, which decides
    /// whether the cheap answer says anything is available, which decides
    /// whether the check escalates into a build.
    struct Harness {
        _dir: tempfile::TempDir,
        flake: PathBuf,
        checker: Checker,
        log: crate::log::Capture,
    }

    fn lock_with(rev: &str) -> String {
        format!(
            r#"{{"version":7,"root":"root","nodes":{{
                 "root":{{"inputs":{{"nixpkgs":"nixpkgs"}}}},
                 "nixpkgs":{{"locked":{{"rev":"{rev}","type":"github"}}}}
               }}}}"#
        )
    }

    const BEFORE_REV: &str = "0e251e2000000000000000000000000000000000";
    const AFTER_REV: &str = "e5bdc4a000000000000000000000000000000000";

    fn harness(updated: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let flake = dir.path().join("flake");
        std::fs::create_dir_all(&flake).unwrap();
        std::fs::write(flake.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        std::fs::write(flake.join("flake.lock"), lock_with(BEFORE_REV)).unwrap();

        let cfg = Config {
            flake: flake.to_string_lossy().into_owned(),
            host: "somehost".into(),
            cache_dir: dir.path().join("cache").to_string_lossy().into_owned(),
            ..Config::default()
        };

        let runner = Runner::fake();
        let f = runner.as_fake();
        f.on("git ls-files", "");
        f.on(
            &format!("git -C {} ls-files -z", flake.display()),
            "flake.nix\0flake.lock\0",
        );

        // `nix flake update` rewrites the scratch lock, exactly as the real one
        // does. Whether it moves is what the test is really configuring.
        let work = dir.path().join("cache/updates/flake");
        let rev = if updated { AFTER_REV } else { BEFORE_REV }.to_string();
        let work_for_update = work.clone();
        f.on_func("nix flake update", move |_| {
            let _ = std::fs::write(work_for_update.join("flake.lock"), lock_with(&rev));
            (Output::default(), Ok(()))
        });

        // `nix build` produces the out-link the diff then resolves.
        let built = dir.path().join("built");
        std::fs::create_dir_all(&built).unwrap();
        let result_link = dir.path().join("cache/updates/result");
        f.on_func("nix build", move |_| {
            let _ = std::fs::remove_file(&result_link);
            let _ = std::os::unix::fs::symlink(&built, &result_link);
            (Output::default(), Ok(()))
        });

        f.on_func("nix path-info", |c| {
            let path = c.args.last().unwrap();
            // The running system has one version of hello; the built one has
            // the next. Enough for the diff to be non-empty and stable.
            let body = if path.contains("aaa-system") {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-1.0":{"narSize":100}}}"#
            } else {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-2.0":{"narSize":150}}}"#
            };
            (
                Output {
                    stdout: body.as_bytes().to_vec(),
                    ..Default::default()
                },
                Ok(()),
            )
        });

        let mut store = Store::new(cfg.updates_dir().unwrap());
        store.current_system = Arc::new(|| Ok(RUNNING.into()));
        let (logger, log) = crate::log::Capture::logger("debug");

        Harness {
            _dir: dir,
            flake,
            checker: Checker::new(cfg, runner, store, logger),
            log,
        }
    }

    impl Harness {
        async fn run(&self) -> Result<()> {
            self.checker.run(&CancellationToken::new()).await
        }
        fn fake(&self) -> &snout_common::run::Fake {
            self.checker.runner.as_fake()
        }
        fn quick(&self) -> Option<Quick> {
            self.checker.store.read_quick()
        }
        fn full(&self) -> Option<Full> {
            self.checker.store.read_full()
        }
        /// Moves the running system out from under the check.
        fn set_system(&mut self, path: &'static str) {
            let probe: SystemProbe = Arc::new(move || Ok(path.into()));
            self.checker.current_system = Arc::clone(&probe);
            self.checker.store.current_system = probe;
        }
    }

    // -- the escalation decision -------------------------------------------

    #[tokio::test]
    async fn nothing_moved_stops_at_the_cheap_answer() {
        let h = harness(false);
        h.run().await.unwrap();
        assert!(!h.quick().unwrap().available);
        assert!(h.full().is_none());
        // The whole point: no build when there is nothing to build.
        assert!(!h.fake().ran("nix build"), "{:?}", h.fake().calls());
    }

    #[tokio::test]
    async fn something_moved_escalates_into_a_build() {
        // The expensive answer is computed the moment the cheap one knows it is
        // needed, hours before anyone asks to read it.
        let h = harness(true);
        h.run().await.unwrap();
        let q = h.quick().unwrap();
        assert!(q.available);
        assert_eq!(q.moved_count, 1);
        assert_eq!(h.full().unwrap().summary.count, 1);
        assert_eq!(h.fake().count_calls("nix build"), 1);
    }

    #[tokio::test]
    async fn the_phases_are_reported_in_the_order_they_run() {
        let mut h = harness(true);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        h.checker.progress = Arc::new(move |p| s.lock().unwrap().push(p));
        h.run().await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Phase::Copying,
                Phase::Updating,
                Phase::Building,
                Phase::Comparing
            ]
        );
    }

    #[tokio::test]
    async fn a_check_that_stops_at_the_cheap_answer_never_reports_building() {
        let mut h = harness(false);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        h.checker.progress = Arc::new(move |p| s.lock().unwrap().push(p));
        h.run().await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![Phase::Copying, Phase::Updating]);
    }

    #[tokio::test]
    async fn a_current_package_diff_is_reused_rather_than_rebuilt() {
        let h = harness(true);
        h.run().await.unwrap();
        assert_eq!(h.fake().count_calls("nix build"), 1);

        // Second run: same lock, same system. Building again could not produce
        // a different answer.
        h.run().await.unwrap();
        assert_eq!(h.fake().count_calls("nix build"), 1, "rebuilt needlessly");
        assert_eq!(h.full().unwrap().summary.count, 1);
    }

    #[tokio::test]
    async fn a_reused_answer_is_restamped_so_its_age_is_honest() {
        let h = harness(true);
        h.run().await.unwrap();
        let first = h.full().unwrap().checked_at;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        h.run().await.unwrap();
        let second = h.full().unwrap().checked_at;
        assert!(second > first, "{second} !> {first}");
    }

    // -- failure handling --------------------------------------------------

    #[tokio::test]
    async fn a_failed_build_is_recorded_against_its_lock_and_not_retried() {
        // A broken nixpkgs fails the same way every hour, and re-downloading
        // gigabytes to reach the same error is not diligence.
        let h = harness(true);
        h.fake()
            .fail("nix build", 1, "error: build of toplevel failed");

        let err = h.run().await.unwrap_err();
        assert_eq!(err.code, Code::Build);
        assert_eq!(h.fake().count_calls("nix build"), 1);

        h.run().await.unwrap();
        assert_eq!(
            h.fake().count_calls("nix build"),
            1,
            "retried a known failure"
        );
    }

    #[tokio::test]
    async fn upstream_moving_again_retries_a_lock_that_failed() {
        // The marker is keyed on the lock rather than being a flag, precisely
        // so a new lock is tried without anyone having to clear anything.
        let h = harness(true);
        h.fake().fail("nix build", 1, "error: boom");
        h.run().await.unwrap_err();
        assert_eq!(h.fake().count_calls("nix build"), 1);

        // A different upstream revision means a different lock hash.
        let work = h.checker.store.work_path();
        h.fake().on_func("nix flake update", move |_| {
            let _ = std::fs::write(
                work.join("flake.lock"),
                lock_with("cccccccccccccccccccccccccccccccccccccccc"),
            );
            (Output::default(), Ok(()))
        });
        h.run().await.unwrap_err();
        assert_eq!(h.fake().count_calls("nix build"), 2, "did not retry");
    }

    #[tokio::test]
    async fn a_cancelled_build_is_not_recorded_as_a_failed_lock() {
        // A daemon shutting down mid-build is not a verdict on the lock.
        // Recording one would stop escalation from ever retrying it.
        let h = harness(true);
        h.fake().fail("nix build", 1, "interrupted");
        let cancel = CancellationToken::new();
        cancel.cancel();

        h.checker.run(&cancel).await.unwrap_err();
        assert_eq!(h.checker.store.failed_lock(), "");
    }

    #[tokio::test]
    async fn reusing_a_diff_clears_a_stale_failure_marker() {
        // The answer being reused was produced by a build that worked. Leaving
        // the marker would suppress escalation for this lock forever.
        let h = harness(true);
        h.run().await.unwrap();
        h.checker.store.set_failed_lock("some-old-hash");

        h.run().await.unwrap();
        assert_eq!(h.checker.store.failed_lock(), "");
    }

    #[tokio::test]
    async fn a_network_failure_preserves_the_last_good_answer() {
        // Losing a last-known-good answer because the network was down is worse
        // than showing one a few hours stale.
        let h = harness(true);
        h.run().await.unwrap();
        let good = h.quick().unwrap();

        h.fake()
            .fail("nix flake update", 1, "error: unable to download");
        let err = h.run().await.unwrap_err();
        assert_eq!(err.code, Code::Network);
        assert_eq!(h.quick().unwrap(), good);
    }

    #[tokio::test]
    async fn a_flake_with_no_tracked_files_says_so() {
        let h = harness(false);
        h.fake()
            .on(&format!("git -C {} ls-files -z", h.flake.display()), "");
        let err = h.run().await.unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(err.to_string().contains("no tracked files"), "{err}");
    }

    #[tokio::test]
    async fn a_tree_whose_tracked_files_are_all_deleted_says_the_same_thing() {
        // Counting listed names rather than copied ones lets this slip past the
        // guard and fail later with a confusing message about flake.lock.
        let h = harness(false);
        h.fake().on(
            &format!("git -C {} ls-files -z", h.flake.display()),
            "gone-a\0gone-b\0",
        );
        let err = h.run().await.unwrap_err();
        assert_eq!(err.code, Code::Config);
        assert!(err.to_string().contains("no tracked files"), "{err}");
    }

    // -- the working tree --------------------------------------------------

    #[tokio::test]
    async fn the_users_own_lock_is_never_touched() {
        // A check must never be something you have to `git checkout`
        // afterwards.
        let h = harness(true);
        let before = std::fs::read_to_string(h.flake.join("flake.lock")).unwrap();
        h.run().await.unwrap();
        let after = std::fs::read_to_string(h.flake.join("flake.lock")).unwrap();
        assert_eq!(before, after);
        // And the update ran against the scratch copy, not the real path.
        let update = h
            .fake()
            .calls()
            .into_iter()
            .find(|c| c.starts_with("nix flake update"))
            .unwrap();
        assert!(update.contains("updates/flake"), "{update}");
        assert!(!update.contains(&format!("--flake {}", h.flake.display())));
    }

    #[tokio::test]
    async fn the_scratch_copy_holds_only_tracked_files() {
        // A recursive copy would drag in .git, which on a real configuration
        // repo is two orders of magnitude larger than the flake itself.
        let h = harness(false);
        std::fs::create_dir_all(h.flake.join(".git")).unwrap();
        std::fs::write(h.flake.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(h.flake.join("untracked.txt"), "scratch").unwrap();

        h.run().await.unwrap();

        let work = h.checker.store.work_path();
        assert!(work.join("flake.nix").exists());
        assert!(!work.join(".git").exists());
        assert!(!work.join("untracked.txt").exists());
    }

    // -- the publish guard -------------------------------------------------

    #[tokio::test]
    async fn an_answer_is_discarded_when_the_system_moves_mid_check() {
        // A deploy landed while the check ran. Publishing now would put an
        // answer about the previous generation back, looking current.
        let mut h = harness(true);
        h.fake().on_func("nix flake update", {
            let work = h.checker.store.work_path();
            move |_| {
                let _ = std::fs::write(work.join("flake.lock"), lock_with(AFTER_REV));
                (Output::default(), Ok(()))
            }
        });
        h.set_system(RUNNING);
        // The probe the checker read at the start still says RUNNING; the store
        // now reports the new generation, as it would after a switch.
        let probe: SystemProbe = Arc::new(|| Ok("/nix/store/zzz-new-system".into()));
        h.checker.store.current_system = probe;

        h.run().await.unwrap();
        assert!(
            h.quick().is_none(),
            "an answer about a system that stopped running was published"
        );
    }

    // -- the journal -------------------------------------------------------

    #[tokio::test]
    async fn the_journal_says_what_the_check_found() {
        let h = harness(true);
        h.run().await.unwrap();
        let lines = h.log.lines();
        assert!(lines.iter().any(|l| l == "1 inputs moved"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "building the updated system"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "1 packages would change"),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn nix_output_reaches_the_journal_only_at_debug_level() {
        // On a cold nixpkgs this is thousands of lines, and a journal that
        // fills with build output is one nobody reads the warnings in.
        let dir = tempfile::tempdir().unwrap();
        let flake = dir.path().join("flake");
        std::fs::create_dir_all(&flake).unwrap();
        std::fs::write(flake.join("flake.nix"), "{}").unwrap();
        std::fs::write(flake.join("flake.lock"), lock_with(BEFORE_REV)).unwrap();

        let cfg = Config {
            flake: flake.to_string_lossy().into_owned(),
            host: "somehost".into(),
            cache_dir: dir.path().join("cache").to_string_lossy().into_owned(),
            ..Config::default()
        };
        let runner = Runner::fake();
        let f = runner.as_fake();
        f.on(
            &format!("git -C {} ls-files -z", flake.display()),
            "flake.nix\0flake.lock\0",
        );
        let work = dir.path().join("cache/updates/flake");
        f.on_func("nix flake update", move |_| {
            let _ = std::fs::write(work.join("flake.lock"), lock_with(BEFORE_REV));
            (
                Output {
                    stderr: b"copying 'github:nixos/nixpkgs'\n\n   \nwarning: dirty tree\n"
                        .to_vec(),
                    ..Default::default()
                },
                Ok(()),
            )
        });

        let mut store = Store::new(cfg.updates_dir().unwrap());
        store.current_system = Arc::new(|| Ok(RUNNING.into()));
        let (logger, log) = crate::log::Capture::logger("info");
        let checker = Checker::new(cfg, runner, store, logger);
        checker.run(&CancellationToken::new()).await.unwrap();

        let lines = log.lines();
        assert!(
            !lines.iter().any(|l| l.contains("github:nixos/nixpkgs")),
            "nix output reached the journal at info level: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "inputs are up to date"),
            "{lines:?}"
        );
    }
}
