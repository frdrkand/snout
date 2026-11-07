//! Restarting the desktop components a system switch leaves running with a
//! stale configuration.
//!
//! It restarts what it is told to, unconditionally. There is deliberately no
//! staleness detection: deciding whether a reload is wanted is the user's call,
//! and the alternative — comparing each running process's argv against the
//! wrapper the current system would launch — was a large amount of fragile,
//! desktop-specific machinery for a question the user can answer instantly.
//!
//! Nothing here knows what any particular compositor or notification daemon is.
//! The component list, the launcher, and the session guard are all
//! configuration.

use std::sync::Arc;
use std::time::Duration;

use snout_common::config::{Component, KIND_COMMAND, KIND_DAEMON, KIND_SYSTEMD_USER, ReloadConfig};
use snout_common::fail::Error;
use snout_common::run::{Cmd, Runner};

/// Bounds how long a daemon is given to exit before it is started again anyway.
///
/// The old instance has to be gone before the new one starts — single-instance
/// daemons hold a socket — so this waits for it rather than racing it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);

pub type Getenv = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct Reloader {
    pub cfg: ReloadConfig,
    pub runner: Runner,
    pub getenv: Getenv,
    /// Injected so a test can count waits instead of taking two real seconds.
    pub sleep: Arc<dyn Fn(Duration) + Send + Sync>,
}

/// One component that did not come back.
#[derive(Debug)]
pub struct Failure {
    pub name: String,
    pub error: String,
}

/// What a reload did, for the caller to print.
#[derive(Debug, Default)]
pub struct Report {
    pub restarted: Vec<String>,
    pub failed: Vec<Failure>,
    pub notes: Vec<String>,
}

impl Reloader {
    pub fn new(cfg: ReloadConfig, runner: Runner) -> Reloader {
        Reloader {
            cfg,
            runner,
            getenv: Arc::new(|k| std::env::var(k).ok().filter(|v| !v.is_empty())),
            sleep: Arc::new(std::thread::sleep),
        }
    }

    /// Restarts every configured component, then runs the post commands.
    ///
    /// A component that fails does not abort the rest: leaving the remaining
    /// daemons un-restarted because the first one was not installed would be
    /// worse than an incomplete reload, and the report names what went wrong.
    pub async fn run(&self) -> (Report, Option<Error>) {
        let mut rep = Report::default();

        if self.cfg.components.is_empty() && self.cfg.post.is_empty() {
            return (
                rep,
                Some(Error::config("no reload components are configured")),
            );
        }

        if let Some(env) = self.session_missing() {
            return (
                rep,
                Some(Error::precondition(format!(
                    "no session here — {env} is not set, so there is nothing to reload into"
                ))),
            );
        }

        for comp in &self.cfg.components {
            match self.one(comp).await {
                Ok(()) => rep.restarted.push(comp.name.clone()),
                Err(e) => rep.failed.push(Failure {
                    name: comp.name.clone(),
                    error: e,
                }),
            }
        }

        for post in &self.cfg.post {
            let (_, res) = self.runner.run(&Cmd::new(post.command.clone())).await;
            match res {
                Ok(()) => rep.restarted.push(post.name.clone()),
                Err(e) => rep.failed.push(Failure {
                    name: post.name.clone(),
                    error: e.to_string(),
                }),
            }
        }

        rep.notes = self.cfg.notes.clone();

        let err = (!rep.failed.is_empty()).then(|| {
            Error::internal(format!(
                "{} of {} reload steps failed",
                rep.failed.len(),
                rep.failed.len() + rep.restarted.len()
            ))
        });
        (rep, err)
    }

    /// The environment variable that is required but unset, if any.
    ///
    /// The launcher generally talks to a compositor, so running outside that
    /// session would start things into nothing.
    fn session_missing(&self) -> Option<&str> {
        let env = &self.cfg.require_env;
        if env.is_empty() || (self.getenv)(env).is_some() {
            None
        } else {
            Some(env)
        }
    }

    async fn one(&self, comp: &Component) -> std::result::Result<(), String> {
        match comp.kind.as_str() {
            KIND_DAEMON => self.restart_daemon(&comp.name).await,
            KIND_SYSTEMD_USER => {
                let unit = if comp.unit.is_empty() {
                    &comp.name
                } else {
                    &comp.unit
                };
                let cmd = Cmd::new(["systemctl", "--user", "restart", unit]);
                let (_, res) = self.runner.run(&cmd).await;
                res.map_err(|e| e.to_string())
            }
            KIND_COMMAND => {
                for argv in [&comp.stop, &comp.start] {
                    if argv.is_empty() {
                        continue;
                    }
                    let (_, res) = self.runner.run(&Cmd::new(argv.clone())).await;
                    res.map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            other => Err(format!("unknown component kind {other:?}")),
        }
    }

    /// Stops a running instance, waits for it to go, and starts a fresh one.
    ///
    /// A component that was not running is started rather than skipped, so a
    /// reload also recovers a daemon that died some time during the session.
    async fn restart_daemon(&self, name: &str) -> std::result::Result<(), String> {
        if self.runner.look_path(name).is_none() {
            return Err(format!("{name} is not on PATH"));
        }

        let re = proc_regex(name);

        // A non-zero status here means nothing matched, which is not a failure:
        // see above.
        let _ = self.runner.run(&Cmd::new(["pkill", "-x", &re])).await;

        let mut remaining = STOP_TIMEOUT;
        while !remaining.is_zero() {
            let (_, res) = self.runner.run(&Cmd::new(["pgrep", "-x", &re])).await;
            if res.is_err() {
                break; // no longer running
            }
            (self.sleep)(POLL);
            remaining = remaining.saturating_sub(POLL);
        }

        self.spawn(name).await
    }

    /// Launches a program through the configured template, so it is parented
    /// the way the session parents it at login and outlives the terminal the
    /// reload was typed into.
    async fn spawn(&self, name: &str) -> std::result::Result<(), String> {
        if self.cfg.spawn.is_empty() {
            return Err("no reload.spawn template configured".into());
        }
        let argv: Vec<String> = self
            .cfg
            .spawn
            .iter()
            .map(|a| a.replace("{}", name))
            .collect();
        let (_, res) = self.runner.run(&Cmd::new(argv)).await;
        res.map_err(|e| e.to_string())
    }

    /// Whether a post-switch "you may want to reload" line is worth printing:
    /// there has to be something to reload, and a session to reload it into.
    ///
    /// Post commands count as something to reload. A configuration with only
    /// post entries is one `reload` runs happily, so hinting about it — and
    /// running it after a deploy — has to agree.
    pub fn hint_applies(&self) -> bool {
        self.cfg.hint_after_deploy && self.has_work() && self.session_missing().is_none()
    }

    /// Whether there is anything at all for a reload to do.
    pub fn has_work(&self) -> bool {
        !self.cfg.components.is_empty() || !self.cfg.post.is_empty()
    }
}

/// Builds the pattern that matches a daemon's process name.
///
/// pkill and pgrep match on comm, which is the name of the *real* executable —
/// and nixpkgs wraps some programs, so a wrapped `foo` runs as `.foo-wrapped`
/// and a plain `pkill -x foo` finds nothing. Left uncorrected that is not a
/// missing status line but a duplicate daemon: the old instance survives and a
/// second one starts beside it.
///
/// comm is also truncated to 15 characters (`.long-name-wrapped` arrives as
/// `.long-name-wrap`), hence the loose tail rather than a literal `-wrapped`.
///
/// The name is escaped before it goes in. It comes from configuration, and an
/// unescaped `.` or `*` would either miss the process or — far worse for a
/// pattern handed to `pkill` — match far more than it should.
pub fn proc_regex(name: &str) -> String {
    format!(r"\.?{}(-wra.*)?", escape(name))
}

/// Escapes every character POSIX extended regular expressions treat specially.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r".^$*+?()[]{}|\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use snout_common::config::PostCmd;
    use std::sync::Mutex;

    fn comp(name: &str, kind: &str) -> Component {
        Component {
            name: name.into(),
            kind: kind.into(),
            unit: String::new(),
            stop: Vec::new(),
            start: Vec::new(),
        }
    }

    /// A configuration in the shape a compositor session actually needs,
    /// expressed declaratively — the component list, the launcher and the
    /// session guard are all data.
    fn session_config() -> ReloadConfig {
        ReloadConfig {
            require_env: "WAYLAND_DISPLAY".into(),
            spawn: vec!["swaymsg".into(), "exec".into(), "--".into(), "{}".into()],
            hint_after_deploy: true,
            after_deploy: false,
            notes: vec!["Compositor config changes need a re-login".into()],
            components: vec![
                comp("statusbar", KIND_DAEMON),
                comp("notifier", KIND_DAEMON),
                comp("polkit-agent", KIND_SYSTEMD_USER),
            ],
            post: vec![PostCmd {
                name: "compositor config".into(),
                command: vec!["swaymsg".into(), "reload".into()],
            }],
        }
    }

    struct Harness {
        reloader: Reloader,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    fn harness(cfg: ReloadConfig, session: bool) -> Harness {
        let runner = Runner::fake();
        let f = runner.as_fake();
        f.on("pkill", "");
        // Non-zero from pgrep means "gone", which is the common case.
        f.fail("pgrep", 1, "");
        f.on("systemctl", "");
        f.on("swaymsg", "");

        let sleeps = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&sleeps);
        let mut r = Reloader::new(cfg, runner);
        r.sleep = Arc::new(move |d| sink.lock().unwrap().push(d));
        r.getenv =
            Arc::new(move |k| (session && k == "WAYLAND_DISPLAY").then(|| "wayland-1".to_string()));

        Harness {
            reloader: r,
            sleeps,
        }
    }

    impl Harness {
        fn fake(&self) -> &snout_common::run::Fake {
            self.reloader.runner.as_fake()
        }
    }

    #[tokio::test]
    async fn components_are_restarted_in_order_then_the_post_commands() {
        let h = harness(session_config(), true);
        let (rep, err) = h.reloader.run().await;
        assert!(err.is_none(), "{:?}", rep.failed);
        assert_eq!(
            rep.restarted,
            ["statusbar", "notifier", "polkit-agent", "compositor config"]
        );
        assert_eq!(rep.notes, ["Compositor config changes need a re-login"]);
    }

    #[tokio::test]
    async fn a_daemon_is_stopped_before_it_is_started() {
        // A single-instance daemon holds a socket; starting the new one first
        // gets it a bind failure.
        let mut cfg = session_config();
        cfg.components = vec![comp("statusbar", KIND_DAEMON)];
        cfg.post.clear();
        let h = harness(cfg, true);
        h.reloader.run().await;

        let calls = h.fake().calls();
        let pkill = calls.iter().position(|c| c.starts_with("pkill")).unwrap();
        let spawn = calls.iter().position(|c| c.starts_with("swaymsg")).unwrap();
        assert!(pkill < spawn, "{calls:?}");
    }

    #[tokio::test]
    async fn the_process_pattern_tolerates_a_nixpkgs_wrapper() {
        // pkill matches on comm, and a wrapped program runs as `.name-wrapped`.
        // Left uncorrected this is not a missing status line but a duplicate
        // daemon: the old instance survives and a second starts beside it.
        assert_eq!(proc_regex("mako"), r"\.?mako(-wra.*)?");
        // comm truncates at 15 characters, so the tail must stay loose.
        assert_eq!(proc_regex("statusbar"), r"\.?statusbar(-wra.*)?");
    }

    #[test]
    fn a_component_name_is_escaped_before_it_reaches_pkill() {
        // The name is configuration. Unescaped, a `.` merely mismatches — but a
        // pattern like `.*` handed to `pkill -x` matches everything running.
        assert_eq!(proc_regex(".*"), r"\.?\.\*(-wra.*)?");
        assert_eq!(proc_regex("foo.bar"), r"\.?foo\.bar(-wra.*)?");
        assert_eq!(proc_regex("a|b"), r"\.?a\|b(-wra.*)?");
        assert_eq!(proc_regex("x(y)"), r"\.?x\(y\)(-wra.*)?");
    }

    #[tokio::test]
    async fn a_daemon_that_lingers_is_waited_for() {
        let mut cfg = session_config();
        cfg.components = vec![comp("statusbar", KIND_DAEMON)];
        cfg.post.clear();
        let h = harness(cfg, true);
        // pgrep succeeding means it is still running.
        h.fake().on("pgrep", "1234");

        h.reloader.run().await;

        // The full timeout at 100ms a poll, and it is started anyway.
        let sleeps = h.sleeps.lock().unwrap().clone();
        assert_eq!(sleeps.len(), 20, "{sleeps:?}");
        assert!(h.fake().ran("swaymsg exec -- statusbar"));
    }

    #[tokio::test]
    async fn a_daemon_that_is_not_running_is_started_anyway() {
        // So a reload also recovers a daemon that died during the session.
        let mut cfg = session_config();
        cfg.components = vec![comp("statusbar", KIND_DAEMON)];
        cfg.post.clear();
        let h = harness(cfg, true);
        h.reloader.run().await;
        assert!(h.sleeps.lock().unwrap().is_empty(), "waited for nothing");
        assert!(h.fake().ran("swaymsg exec -- statusbar"));
    }

    #[tokio::test]
    async fn the_spawn_template_substitutes_every_placeholder() {
        let mut cfg = session_config();
        cfg.spawn = vec!["launch".into(), "--name={}".into(), "--exec={}".into()];
        cfg.components = vec![comp("statusbar", KIND_DAEMON)];
        cfg.post.clear();
        let h = harness(cfg, true);
        h.fake().on("launch", "");
        h.reloader.run().await;
        assert!(
            h.fake().ran("launch --name=statusbar --exec=statusbar"),
            "{:?}",
            h.fake().calls()
        );
    }

    #[tokio::test]
    async fn a_systemd_component_uses_its_unit_override() {
        let mut cfg = session_config();
        cfg.components = vec![Component {
            unit: "polkit-gnome-authentication-agent-1".into(),
            ..comp("polkit-agent", KIND_SYSTEMD_USER)
        }];
        cfg.post.clear();
        let h = harness(cfg, true);
        h.reloader.run().await;
        assert!(
            h.fake()
                .ran("systemctl --user restart polkit-gnome-authentication-agent-1")
        );
    }

    #[tokio::test]
    async fn a_command_component_runs_stop_then_start() {
        let mut cfg = session_config();
        cfg.components = vec![Component {
            stop: vec!["pkill".into(), "-x".into(), "wallpaper".into()],
            start: vec!["wallpaper".into(), "--daemon".into()],
            ..comp("wallpaper", KIND_COMMAND)
        }];
        cfg.post.clear();
        let h = harness(cfg, true);
        h.fake().on("wallpaper", "");
        h.reloader.run().await;
        assert_eq!(
            h.fake().calls(),
            ["pkill -x wallpaper", "wallpaper --daemon"]
        );
    }

    #[tokio::test]
    async fn one_failure_does_not_abort_the_rest() {
        // Leaving the remaining daemons un-restarted because the first was not
        // installed would be worse than an incomplete reload.
        let mut cfg = session_config();
        cfg.post.clear();
        let h = harness(cfg, true);
        h.fake().missing("statusbar");

        let (rep, err) = h.reloader.run().await;
        assert!(err.is_some());
        assert_eq!(err.unwrap().to_string(), "1 of 3 reload steps failed");
        assert_eq!(rep.restarted, ["notifier", "polkit-agent"]);
        assert_eq!(rep.failed.len(), 1);
        assert_eq!(rep.failed[0].name, "statusbar");
        assert!(rep.failed[0].error.contains("not on PATH"));
    }

    #[tokio::test]
    async fn the_report_survives_a_failure_so_the_caller_can_print_it() {
        let mut cfg = session_config();
        cfg.post.clear();
        let h = harness(cfg, true);
        h.fake().missing("statusbar");
        let (rep, err) = h.reloader.run().await;
        assert!(err.is_some());
        assert!(!rep.restarted.is_empty(), "the report was thrown away");
        assert_eq!(rep.notes.len(), 1);
    }

    #[tokio::test]
    async fn nothing_configured_is_a_configuration_error() {
        let h = harness(ReloadConfig::default_empty(), true);
        let (_, err) = h.reloader.run().await;
        let err = err.unwrap();
        assert_eq!(err.code, snout_common::fail::Code::Config);
        assert!(err.to_string().contains("no reload components"), "{err}");
    }

    #[tokio::test]
    async fn no_session_is_a_precondition_not_a_failure() {
        // Exit 3, so a script can tell "there was nothing to do here" from
        // "the reload broke".
        let h = harness(session_config(), false);
        let (_, err) = h.reloader.run().await;
        let err = err.unwrap();
        assert_eq!(err.code, snout_common::fail::Code::Precondition);
        assert!(err.to_string().contains("WAYLAND_DISPLAY"), "{err}");
        // And nothing was touched.
        assert!(h.fake().calls().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_kind_fails_only_its_own_component() {
        let mut cfg = session_config();
        cfg.components = vec![comp("mystery", "sorcery"), comp("notifier", KIND_DAEMON)];
        cfg.post.clear();
        let h = harness(cfg, true);
        let (rep, err) = h.reloader.run().await;
        assert!(err.is_some());
        assert_eq!(rep.restarted, ["notifier"]);
        assert!(rep.failed[0].error.contains("unknown component kind"));
    }

    #[test]
    fn the_hint_needs_something_to_reload_and_somewhere_to_reload_it() {
        let cases: &[(&str, bool, bool, bool)] = &[
            // description, hint enabled, session present, expected
            ("everything present", true, true, true),
            ("hint disabled", false, true, false),
            ("no session", true, false, false),
        ];
        for (what, hint, session, want) in cases {
            let mut cfg = session_config();
            cfg.hint_after_deploy = *hint;
            let h = harness(cfg, *session);
            assert_eq!(h.reloader.hint_applies(), *want, "{what}");
        }
    }

    #[test]
    fn a_post_only_configuration_still_counts_as_something_to_reload() {
        // `reload` runs a post-only configuration happily, so the hint — and
        // the automatic reload after a deploy — have to agree, or
        // `afterDeploy = true` silently does nothing.
        let mut cfg = session_config();
        cfg.components.clear();
        let h = harness(cfg, true);
        assert!(h.reloader.has_work());
        assert!(h.reloader.hint_applies());
    }

    #[test]
    fn an_empty_configuration_has_no_work_and_no_hint() {
        let h = harness(ReloadConfig::default_empty(), true);
        assert!(!h.reloader.has_work());
        assert!(!h.reloader.hint_applies());
    }
}
