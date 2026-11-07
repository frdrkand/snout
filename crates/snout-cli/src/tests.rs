//! End-to-end tests over the whole command surface.
//!
//! These drive `run` with a fake runner, buffered streams and — where the
//! command talks to one — a stand-in daemon on a real unix socket. So they
//! exercise argument parsing, dispatch, the config layers, the protocol and the
//! printed output the way a real invocation does, without a NixOS system
//! underneath or a daemon process to start.
//!
//! Half of what is worth testing here is what happens with *no* daemon, because
//! that is the rule the split rests on: every command refuses, except `status`,
//! whose job is to report exactly that.

// The env guard is deliberately held across the awaits: it exists precisely to
// stop two of these running at once, and an async mutex would not serialise
// them against the synchronous env tests elsewhere in the workspace.
#![allow(clippy::await_holding_lock)]
// `Harness::new` builds a scratch machine and mutates the process environment.
// A `Default` impl would invite calling it where that is not expected.
#![allow(clippy::new_without_default)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};

use snout_common::docs::{
    DaemonStatus, ErrorDoc, Full, FullStatus, MovedDoc, Phase, Quick, QuickStatus, SCHEMA_VERSION,
    Status, UpdatesDoc,
};
use snout_common::fail::{Code, EXIT_FAILED, EXIT_OK, EXIT_PRECONDITION, EXIT_USAGE};
use snout_common::lockdiff::Moved;
use snout_common::nix::{Change, Kind, Summary};
use snout_common::proto::{self, Reply, Request, socket_path};
use snout_common::run::{Output, Runner};
use snout_common::testenv::env_guard as guard;
use snout_common::time::now;

use crate::ui::Stream;

/// The running system every end-to-end test pretends to be on.
const RUNNING: &str = "/nix/store/aaa-system";

pub struct Harness {
    _dir: tempfile::TempDir,
    pub flake: PathBuf,
    pub runner: Runner,
    pub out: Stream,
    pub err: Stream,
    daemon: Option<FakeDaemon>,
}

fn lock_with(rev: &str) -> String {
    format!(
        r#"{{"version":7,"root":"root","nodes":{{"root":{{"inputs":{{"nixpkgs":"nixpkgs"}}}},
             "nixpkgs":{{"locked":{{"rev":"{rev}","type":"github"}}}}}}}}"#
    )
}

const BEFORE_REV: &str = "0e251e2000000000000000000000000000000000";
const AFTER_REV: &str = "e5bdc4a000000000000000000000000000000000";

impl Harness {
    /// A configured machine with a scratch flake and an isolated environment.
    pub fn new() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let flake = dir.path().join("flake");
        std::fs::create_dir_all(&flake).unwrap();
        std::fs::write(flake.join("flake.nix"), "{}").unwrap();
        std::fs::write(flake.join("flake.lock"), lock_with(BEFORE_REV)).unwrap();

        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let cache = dir.path().join("cache");
        let run = dir.path().join("run");
        std::fs::create_dir_all(&run).unwrap();

        // The whole env the config load and the socket path read, pinned. An
        // env layer leaking in from the developer's own shell would silently
        // override the fixture — or, worse, point the client at the developer's
        // own running daemon.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            std::env::set_var("XDG_CACHE_HOME", &cache);
            std::env::set_var("XDG_RUNTIME_DIR", &run);
            std::env::set_var("SNOUT_CONFIG", dir.path().join("system.json"));
            std::env::set_var("NO_COLOR", "1");
            std::env::remove_var("SNOUT_FLAKE");
            std::env::remove_var("SNOUT_HOST");
            std::env::remove_var("SNOUT_LOG_LEVEL");
        }
        std::fs::write(
            dir.path().join("system.json"),
            serde_json::json!({
                "flake": flake.to_string_lossy(),
                "host": "somehost",
                "cacheDir": cache.to_string_lossy(),
            })
            .to_string(),
        )
        .unwrap();

        let runner = Runner::fake();
        let f = runner.as_fake();

        // `nix flake update` rewrites the *real* lock here, because that is
        // what `upgrade` runs it on — the daemon's scratch-copy trick is the
        // daemon's, and this half genuinely does move the user's file.
        let real_lock = flake.join("flake.lock");
        f.on_func("nix flake update", move |_| {
            let _ = std::fs::write(&real_lock, lock_with(AFTER_REV));
            (Output::default(), Ok(()))
        });

        let built = dir.path().join("built");
        std::fs::create_dir_all(&built).unwrap();
        let build_link = cache.join("snout/build/result");
        f.on_func("nix build", move |_| {
            if let Some(p) = build_link.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::remove_file(&build_link);
            let _ = std::os::unix::fs::symlink(&built, &build_link);
            (Output::default(), Ok(()))
        });

        f.on_func("nix path-info", |c| {
            let last = c.args.last().unwrap();
            let body = if last.contains("current-system") || last.contains("aaa-system") {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-1.0":{"narSize":100}}}"#
            } else {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-2.0":{"narSize":9000000}}}"#
            };
            (
                Output {
                    stdout: body.as_bytes().to_vec(),
                    ..Default::default()
                },
                Ok(()),
            )
        });
        f.on("sudo", "");

        Harness {
            _dir: dir,
            flake,
            runner,
            out: Stream::buffer(),
            err: Stream::buffer(),
            daemon: None,
        }
    }

    /// Starts a stand-in daemon on the socket the CLI will dial.
    fn with_daemon(
        mut self,
        answer: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Harness {
        self.daemon = Some(FakeDaemon::start(Arc::new(answer)));
        self
    }

    /// The common case: a daemon that answers from a fixed set of documents.
    fn with_cache(self, st: Status, updates: UpdatesDoc, moved: MovedDoc) -> Harness {
        self.with_daemon(move |req| match req {
            Request::Status => Reply::Status(Box::new(st.clone())),
            Request::ListUpdates => Reply::Updates(updates.clone()),
            Request::ListMoved => Reply::Moved(moved.clone()),
            Request::Invalidate => Reply::Ok,
            Request::Check => Reply::Check { started: true },
        })
    }

    /// A daemon that is up but has never completed a check.
    fn with_cold_cache(self) -> Harness {
        self.with_cache(status(), UpdatesDoc::empty(), MovedDoc::empty())
    }

    pub async fn run(&self, args: &[&str]) -> u8 {
        let mut argv: Vec<OsString> = vec!["snout".into()];
        argv.extend(args.iter().map(OsString::from));
        crate::cli::run(
            argv,
            self.out.clone(),
            self.err.clone(),
            self.runner.clone(),
        )
        .await
    }

    pub fn stdout(&self) -> String {
        self.out.contents()
    }
    pub fn stderr(&self) -> String {
        self.err.contents()
    }
    pub fn fake(&self) -> &snout_common::run::Fake {
        self.runner.as_fake()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout())
            .unwrap_or_else(|e| panic!("stdout was not one JSON document: {e}\n{}", self.stdout()))
    }
    /// Every request the CLI actually made, in order.
    fn asked(&self) -> Vec<Request> {
        self.daemon
            .as_ref()
            .map(|d| d.seen.lock().unwrap().clone())
            .unwrap_or_default()
    }
    fn lock(&self) -> String {
        std::fs::read_to_string(self.flake.join("flake.lock")).unwrap()
    }
}

// ---------------------------------------------------------------------------
// The stand-in daemon
// ---------------------------------------------------------------------------

struct FakeDaemon {
    seen: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeDaemon {
    fn start(answer: Arc<dyn Fn(&Request) -> Reply + Send + Sync>) -> FakeDaemon {
        let path = socket_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                let answer = Arc::clone(&answer);
                let log = Arc::clone(&log);
                tokio::spawn(async move { serve_one(conn, answer, log).await });
            }
        });
        FakeDaemon { seen, task }
    }
}

async fn serve_one(
    conn: UnixStream,
    answer: Arc<dyn Fn(&Request) -> Reply + Send + Sync>,
    log: Arc<Mutex<Vec<Request>>>,
) {
    let (r, mut w) = conn.into_split();
    let mut reader = BufReader::new(r);
    // A bare connect-and-close is how `require` probes; answering nothing is
    // exactly what the real daemon does.
    let Ok(Some(req)) = proto::read_line::<Request, _>(&mut reader).await else {
        return;
    };
    log.lock().unwrap().push(req.clone());
    let reply = answer(&req);
    let _ = proto::write_line(&mut w, &reply).await;
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn status() -> Status {
    Status {
        schema_version: SCHEMA_VERSION,
        version: "1.2.3 (abc1234)".into(),
        host: "somehost".into(),
        flake: "/etc/nixos".into(),
        daemon: DaemonStatus {
            running: true,
            busy: false,
            next_check_at: Some(now() + chrono::TimeDelta::hours(1)),
            backoff_until: None,
            phase: None,
            started_at: None,
        },
        quick: QuickStatus::default(),
        full: FullStatus::default(),
        error: None,
    }
}

fn warm_status() -> Status {
    Status {
        quick: QuickStatus {
            checked_at: Some(now()),
            available: true,
            moved_count: 1,
            moved: vec![moved_input()],
        },
        full: FullStatus {
            checked_at: Some(now()),
            summary: summary(),
        },
        ..status()
    }
}

fn moved_input() -> Moved {
    Moved {
        name: "nixpkgs".into(),
        from: "0e251e2".into(),
        to: "e5bdc4a".into(),
    }
}

fn summary() -> Summary {
    Summary {
        count: 2,
        changed: 1,
        added: 1,
        removed: 0,
        size_only: 0,
        total_size_delta: 2_411_724,
    }
}

fn warm_updates() -> UpdatesDoc {
    UpdatesDoc::from(Some(Full {
        schema_version: SCHEMA_VERSION,
        checked_at: now(),
        lock_hash: "h".into(),
        current_system: RUNNING.into(),
        host: "somehost".into(),
        flake: "/etc/nixos".into(),
        summary: summary(),
        updates: vec![
            Change {
                name: "hello".into(),
                from: "1.0".into(),
                to: "2.0".into(),
                kind: Kind::Changed,
                size_delta: 8_999_900,
            },
            Change {
                name: "ripgrep".into(),
                from: String::new(),
                to: "14.1".into(),
                kind: Kind::Added,
                size_delta: 100,
            },
        ],
    }))
}

fn warm_moved() -> MovedDoc {
    MovedDoc::from(Some(Quick {
        schema_version: SCHEMA_VERSION,
        checked_at: now(),
        available: true,
        moved_count: 1,
        moved: vec![moved_input()],
        lock_hash: "h".into(),
        current_system: RUNNING.into(),
        host: "somehost".into(),
        flake: "/etc/nixos".into(),
    }))
}

// ---------------------------------------------------------------------------
// The command table
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn help_lists_the_eight_commands_and_hides_completion() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["--help"]).await, EXIT_OK);
    let out = h.stdout();
    for name in [
        "build",
        "deploy",
        "upgrade",
        "list-updates",
        "has-updates",
        "check",
        "status",
        "reload",
    ] {
        assert!(out.contains(name), "{name} missing from help:\n{out}");
    }
    // Present as a command, absent from the listing: it exists for the
    // packaging to call, not for anyone to type.
    assert!(!out.contains("completion"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_commands_that_no_longer_exist_are_usage_errors() {
    // They were real commands until this split; a stale muscle-memory
    // invocation should say so rather than doing something surprising.
    let _g = guard();
    // `check` is not in this list: it came back, as a request to the daemon
    // rather than a check run in this process.
    for gone in ["diff", "rollback", "debug", "version", "update"] {
        let h = Harness::new();
        assert_eq!(h.run(&[gone]).await, EXIT_USAGE, "{gone} still runs");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn json_is_refused_by_the_commands_that_cannot_produce_it() {
    let _g = guard();
    for cmd in ["build", "deploy", "upgrade", "reload", "check"] {
        let h = Harness::new();
        assert_eq!(h.run(&[cmd, "--json"]).await, EXIT_USAGE, "{cmd}");
        // Reported *as* JSON, because --json was asked for: stdout carries one
        // document even when the answer is that there will not be one.
        let v = h.json();
        assert_eq!(v["error"]["code"], "usage", "{cmd}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not produce JSON"),
            "{cmd}: {v}"
        );
        // And it was refused before anything ran.
        assert!(h.fake().calls().is_empty(), "{cmd} did work anyway");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_json_command_still_emits_exactly_one_document() {
    // A consumer must never have to distinguish "empty stdout" from "the tool
    // died". Here the failure is the daemon being absent.
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["list-updates", "--json"]).await, EXIT_PRECONDITION);
    let v = h.json();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["error"]["code"], "precondition");
}

#[tokio::test(flavor = "multi_thread")]
async fn even_a_rejected_flag_produces_json_when_json_was_asked_for() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(
        h.run(&["list-updates", "--nonsense", "--json"]).await,
        EXIT_USAGE
    );
    assert_eq!(h.json()["error"]["code"], "usage");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_config_is_a_configuration_error() {
    let _g = guard();
    let h = Harness::new();
    unsafe { std::env::set_var("SNOUT_CONFIG", "/nonexistent/snout.json") };
    // A configuration error exits 2: it is something the reader has to go and
    // fix, like a mistyped flag, not a step that failed on its own.
    assert_eq!(h.run(&["status"]).await, EXIT_USAGE);
    assert!(h.stderr().contains("does not exist"), "{}", h.stderr());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relative_flake_is_rejected_before_anything_runs() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(
        h.run(&["build", "--flake", "relative/path"]).await,
        EXIT_USAGE
    );
    assert!(h.stderr().contains("must be absolute"), "{}", h.stderr());
    assert!(h.fake().calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_override_beats_the_config_file() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["build", "--host", "otherhost"]).await, EXIT_OK);
    assert!(
        h.fake().ran("nix build") && h.fake().calls().iter().any(|c| c.contains("otherhost")),
        "{:?}",
        h.fake().calls()
    );
}

// ---------------------------------------------------------------------------
// No daemon
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn every_command_but_status_refuses_when_the_daemon_is_absent() {
    // The rule the whole split rests on. Each of these needs the daemon for a
    // different reason — three cannot answer without it, three must be able to
    // tell it the system changed — and the message has to name it either way.
    let _g = guard();
    for cmd in [
        "build",
        "deploy",
        "upgrade",
        "list-updates",
        "has-updates",
        "check",
        "reload",
    ] {
        let h = Harness::new();
        assert_eq!(h.run(&[cmd]).await, EXIT_PRECONDITION, "{cmd}");
        let err = h.stderr();
        assert!(err.contains("snout-daemon is not running"), "{cmd}: {err}");
        assert!(err.contains("systemctl --user"), "{cmd}: {err}");
        // Nothing was started. A build that ran here would leave a cache
        // nobody was going to correct.
        assert!(h.fake().calls().is_empty(), "{cmd} shelled out anyway");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_is_the_one_command_that_answers_without_a_daemon() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["status"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("not running"), "{out}");
    assert!(out.contains(snout_common::version::VERSION), "{out}");
    // It says what to do about it.
    assert!(out.contains("systemctl --user start"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_status_makes_no_claim_about_the_cache() {
    // The cache is the daemon's. Reporting "0 to update" with the daemon down
    // would be inventing an answer nothing read.
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["status", "--json"]).await, EXIT_OK);
    let v = h.json();
    assert_eq!(v["daemon"]["running"], false);
    assert!(v.get("quick").is_none(), "{v}");
    assert!(v.get("full").is_none(), "{v}");
    assert!(v["version"].is_string());
}

// ---------------------------------------------------------------------------
// Reading what the daemon knows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_updates_never_shells_out() {
    // The promise that makes this safe to call on a window open: whatever
    // happens, it returns at once and builds nothing.
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["list-updates"]).await, EXIT_OK);
    assert!(h.fake().calls().is_empty(), "{:?}", h.fake().calls());
    assert_eq!(h.asked(), vec![Request::ListUpdates]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cold_cache_says_nothing_checked_rather_than_up_to_date() {
    // "Nothing has been checked" and "nothing has changed" are different
    // answers, and rendering the first as the second is the one thing this
    // command must not do.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["list-updates"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("nothing checked yet"), "{out}");
    assert!(!out.contains("up to date"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cold_cache_answers_json_in_the_same_shape_as_a_warm_one() {
    let _g = guard();

    let cold = Harness::new().with_cold_cache();
    assert_eq!(cold.run(&["list-updates", "--json"]).await, EXIT_OK);
    let cold = cold.json();

    let warm = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(warm.run(&["list-updates", "--json"]).await, EXIT_OK);
    let warm = warm.json();

    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&cold), keys(&warm));
    assert!(cold["checkedAt"].is_null());
    assert_eq!(cold["updates"], serde_json::json!([]));
    assert_eq!(cold["count"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_updates_prints_the_package_table() {
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["list-updates"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("2 updates"), "{out}");
    assert!(out.contains("hello"), "{out}");
    assert!(out.contains("ripgrep"), "{out}");
    assert!(out.contains("closure"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn quick_asks_a_different_question_of_the_daemon() {
    // Not a display filter over the same answer: the two are separate
    // documents, computed at different costs.
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["list-updates", "--quick"]).await, EXIT_OK);
    assert_eq!(h.asked(), vec![Request::ListMoved]);

    let out = h.stdout();
    assert!(out.contains("1 input moved"), "{out}");
    assert!(out.contains("nixpkgs"), "{out}");
    // The package list is not in the cheap answer, and must not be implied.
    assert!(!out.contains("hello"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_prints_one_bare_word() {
    let _g = guard();

    let yes = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(yes.run(&["has-updates"]).await, EXIT_OK);
    assert_eq!(yes.stdout(), "yes\n");

    let no = Harness::new().with_cold_cache();
    assert_eq!(no.run(&["has-updates"]).await, EXIT_OK);
    assert_eq!(no.stdout(), "no\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_never_encodes_its_answer_in_the_exit_status() {
    // "There are updates" is not a failure, and a script doing
    // `snout has-updates && ...` should not be surprised.
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["has-updates"]).await, EXIT_OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_json_carries_what_an_indicator_needs() {
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["has-updates", "--json"]).await, EXIT_OK);
    let v = h.json();
    let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["available", "busy", "checkedAt", "error", "schemaVersion"]
    );
    assert_eq!(v["available"], true);
    assert!(v["checkedAt"].is_string());
    assert_eq!(v["busy"], false);
    assert!(v["error"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_json_says_never_checked_rather_than_no() {
    // `available: false` alone reads the same for "nothing moved" and "nothing
    // checked yet". The null checkedAt is what tells them apart.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["has-updates", "--json"]).await, EXIT_OK);
    let v = h.json();
    assert_eq!(v["available"], false);
    assert!(v["checkedAt"].is_null(), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_json_reports_a_running_check() {
    let _g = guard();
    let mut st = warm_status();
    st.daemon.busy = true;
    let h = Harness::new().with_cache(st, warm_updates(), warm_moved());
    assert_eq!(h.run(&["has-updates", "--json"]).await, EXIT_OK);
    assert_eq!(h.json()["busy"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn has_updates_json_reports_failing_checks_but_not_failing_builds() {
    // A failed fetch means the answer is going stale; a failed package build
    // leaves "have the inputs moved" answered perfectly well.
    let _g = guard();
    let failing = |code: Code| {
        let mut st = warm_status();
        st.error = Some(ErrorDoc {
            code,
            message: "boom".into(),
        });
        st
    };

    let h = Harness::new().with_cache(failing(Code::Network), warm_updates(), warm_moved());
    assert_eq!(h.run(&["has-updates", "--json"]).await, EXIT_OK);
    assert_eq!(h.json()["error"]["code"], "network");

    let h = Harness::new().with_cache(failing(Code::Build), warm_updates(), warm_moved());
    assert_eq!(h.run(&["has-updates", "--json"]).await, EXIT_OK);
    assert!(h.json()["error"].is_null(), "{}", h.json());
}

#[tokio::test(flavor = "multi_thread")]
async fn status_json_carries_the_phase_of_a_running_check() {
    let _g = guard();
    let mut st = warm_status();
    st.daemon.busy = true;
    st.daemon.phase = Some(Phase::Building);
    st.daemon.started_at = Some(now());
    let h = Harness::new().with_cache(st, warm_updates(), warm_moved());
    assert_eq!(h.run(&["status", "--json"]).await, EXIT_OK);
    let v = h.json();
    assert_eq!(v["daemon"]["phase"], "building");
    assert!(v["daemon"]["startedAt"].is_string());

    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["status"]).await, EXIT_OK);
    assert!(!h.stdout().contains("building"), "{}", h.stdout());
}

#[tokio::test(flavor = "multi_thread")]
async fn status_names_the_step_a_running_check_is_on() {
    let _g = guard();
    let mut st = warm_status();
    st.daemon.busy = true;
    st.daemon.phase = Some(Phase::Building);
    st.daemon.started_at = Some(now());
    let h = Harness::new().with_cache(st, warm_updates(), warm_moved());
    assert_eq!(h.run(&["status"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("building the updated system"), "{out}");
    assert!(out.contains("started just now"), "{out}");
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// A daemon whose status reports a check in progress for the first `busy_for`
/// polls, then the given final status.
fn checking(busy_for: usize, done: Status) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
    let polls = std::sync::atomic::AtomicUsize::new(0);
    move |req| match req {
        Request::Check => Reply::Check { started: true },
        Request::Status => {
            let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < busy_for {
                let mut st = done.clone();
                st.daemon.busy = true;
                st.daemon.phase = Some(if n == 0 {
                    Phase::Updating
                } else {
                    Phase::Building
                });
                st.daemon.started_at = Some(now());
                st.error = None;
                Reply::Status(Box::new(st))
            } else {
                Reply::Status(Box::new(done.clone()))
            }
        }
        other => panic!("check asked an unexpected question: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn check_asks_the_daemon_then_follows_it_to_the_answer() {
    let _g = guard();
    let h = Harness::new().with_daemon(checking(2, warm_status()));
    assert_eq!(h.run(&["check"]).await, EXIT_OK);

    assert_eq!(h.asked()[0], Request::Check);
    let out = h.stdout() + &h.stderr();
    assert!(out.contains("updating inputs"), "{out}");
    assert!(out.contains("building the updated system"), "{out}");
    assert!(out.contains("1 input moved"), "{out}");
    assert!(out.contains("2 updates"), "{out}");
    // The daemon did the work; nothing ran here.
    assert!(h.fake().calls().is_empty(), "{:?}", h.fake().calls());
}

#[tokio::test(flavor = "multi_thread")]
async fn check_no_wait_returns_once_the_request_is_in() {
    let _g = guard();
    let h = Harness::new().with_daemon(checking(1000, warm_status()));
    assert_eq!(h.run(&["check", "--no-wait"]).await, EXIT_OK);
    assert_eq!(h.asked(), vec![Request::Check]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_check_exits_with_the_code_the_daemon_recorded() {
    // As if the work had happened here: a failed fetch exits 1.
    let _g = guard();
    let mut done = warm_status();
    done.error = Some(ErrorDoc {
        code: Code::Network,
        message: "updating flake inputs: unable to download".into(),
    });
    let h = Harness::new().with_daemon(checking(1, done));
    assert_eq!(h.run(&["check"]).await, EXIT_FAILED);
    assert!(h.stderr().contains("unable to download"), "{}", h.stderr());
}

#[tokio::test(flavor = "multi_thread")]
async fn check_says_so_when_nothing_moved() {
    let _g = guard();
    let mut done = status();
    done.quick = QuickStatus {
        checked_at: Some(now()),
        ..Default::default()
    };
    let h = Harness::new().with_daemon(checking(0, done));
    assert_eq!(h.run(&["check"]).await, EXIT_OK);
    let out = h.stdout() + &h.stderr();
    assert!(out.contains("inputs up to date"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_daemons_version_not_the_clis() {
    // They are separate binaries; a new `snout` talking to a daemon systemd
    // has not restarted is exactly what this is for.
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["status"]).await, EXIT_OK);
    assert!(h.stdout().contains("1.2.3 (abc1234)"), "{}", h.stdout());
}

#[tokio::test(flavor = "multi_thread")]
async fn status_names_the_daemon_the_inputs_and_the_packages() {
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["status"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("running"), "{out}");
    assert!(out.contains("next check"), "{out}");
    assert!(out.contains("1 moved"), "{out}");
    assert!(out.contains("2 to update"), "{out}");
    assert!(h.fake().calls().is_empty(), "status started work");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_json_is_the_snapshot_verbatim() {
    let _g = guard();
    let h = Harness::new().with_cache(warm_status(), warm_updates(), warm_moved());
    assert_eq!(h.run(&["status", "--json"]).await, EXIT_OK);
    let v = h.json();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["daemon"]["running"], true);
    assert_eq!(v["quick"]["movedCount"], 1);
    assert_eq!(v["full"]["count"], 2);
}

// ---------------------------------------------------------------------------
// Work that happens here
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn build_asks_nix_for_the_toplevel_rather_than_nixos_rebuild() {
    // `nixos-rebuild build` has no --out-link in the ng rewrite, and placing
    // the symlink is the whole point of this command.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["build"]).await, EXIT_OK);

    let calls = h.fake().calls();
    let build = calls.iter().find(|c| c.starts_with("nix build")).unwrap();
    assert!(build.contains("nixosConfigurations.somehost"), "{build}");
    assert!(build.contains("system.build.toplevel"), "{build}");
    assert!(build.contains("--out-link"), "{build}");
    assert!(!calls.iter().any(|c| c.contains("nixos-rebuild")));
}

#[tokio::test(flavor = "multi_thread")]
async fn build_prints_the_diff_against_the_running_system() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["build"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("built"), "{out}");
    assert!(out.contains("hello"), "{out}");
    assert!(out.contains("closure"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_build_exits_one() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    h.fake()
        .fail("nix build", 1, "error: build of toplevel failed");
    assert_eq!(h.run(&["build"]).await, EXIT_FAILED);
    assert!(h.stderr().contains("building"), "{}", h.stderr());
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_uses_the_one_privileged_command_line() {
    // The privileged part is exactly one command, and keeping it that way is
    // what makes it obvious what runs as root.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["deploy"]).await, EXIT_OK);

    let calls = h.fake().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("sudo nixos-rebuild switch --flake "),
        "{calls:?}"
    );
    assert!(calls[0].contains("#somehost"), "{calls:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_tells_the_daemon_the_system_changed() {
    // Otherwise the cache goes on advertising an update that is now installed,
    // for up to a full interval.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["deploy"]).await, EXIT_OK);
    assert!(h.asked().contains(&Request::Invalidate), "{:?}", h.asked());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_switch_never_invalidates_the_cache() {
    // Nothing changed, so the cached answer is still the truth.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    h.fake().fail("sudo", 1, "error: activation failed");
    assert_eq!(h.run(&["deploy"]).await, EXIT_FAILED);
    assert!(!h.asked().contains(&Request::Invalidate), "{:?}", h.asked());
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_updates_the_lock_then_switches() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["upgrade"]).await, EXIT_OK);

    let calls = h.fake().calls();
    assert!(calls[0].starts_with("nix flake update"), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("sudo nixos-rebuild switch")),
        "{calls:?}"
    );
    assert!(h.lock().contains(AFTER_REV));
    assert!(h.asked().contains(&Request::Invalidate));
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_no_deploy_updates_the_lock_and_stops() {
    // Not "build but do not switch": the lock, and nothing else.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["upgrade", "--no-deploy"]).await, EXIT_OK);

    let calls = h.fake().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("nix flake update"), "{calls:?}");
    assert!(h.lock().contains(AFTER_REV));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_rewrite_invalidates_the_cache_even_with_nothing_deployed() {
    // The cached answer was computed against the lock as it was. It has moved,
    // so the answer has stopped describing this flake.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["upgrade", "--no-deploy"]).await, EXIT_OK);
    assert!(h.asked().contains(&Request::Invalidate), "{:?}", h.asked());
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_names_what_moved() {
    // A list of full github:owner/repo/<40 hex> refs is what nix prints, and it
    // is not what a reader wants.
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["upgrade", "--no-deploy"]).await, EXIT_OK);
    let out = h.stdout();
    assert!(out.contains("nixpkgs"), "{out}");
    assert!(out.contains("1 input moved"), "{out}");
    assert!(
        !out.contains(AFTER_REV),
        "the full revision was printed: {out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_says_so_when_the_lock_did_not_move() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    // An update that changes nothing rewrites the file with the same contents.
    let lock = h.flake.join("flake.lock");
    h.fake().on_func("nix flake update", move |_| {
        let _ = std::fs::write(&lock, lock_with(BEFORE_REV));
        (Output::default(), Ok(()))
    });
    assert_eq!(h.run(&["upgrade", "--no-deploy"]).await, EXIT_OK);
    assert!(h.stdout().contains("already up to date"), "{}", h.stdout());
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_with_nothing_configured_is_a_configuration_error() {
    let _g = guard();
    let h = Harness::new().with_cold_cache();
    assert_eq!(h.run(&["reload"]).await, EXIT_USAGE);
    assert!(
        h.stderr().contains("no reload components are configured"),
        "{}",
        h.stderr()
    );
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn completion_scripts_name_every_command() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["completion", "bash"]).await, EXIT_OK);
    let out = h.stdout();
    for name in [
        "build",
        "deploy",
        "upgrade",
        "list-updates",
        "has-updates",
        "check",
        "status",
    ] {
        assert!(out.contains(name), "{name} missing from completions");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn completion_needs_neither_configuration_nor_a_daemon() {
    // The packaging runs it in a build sandbox, where there is neither.
    let _g = guard();
    let h = Harness::new();
    unsafe { std::env::set_var("SNOUT_CONFIG", "/nonexistent/snout.json") };
    assert_eq!(h.run(&["completion", "zsh"]).await, EXIT_OK);
    assert!(!h.stdout().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_shell_is_a_usage_error() {
    let _g = guard();
    let h = Harness::new();
    assert_eq!(h.run(&["completion", "csh"]).await, EXIT_USAGE);
}
