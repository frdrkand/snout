//! The control socket.
//!
//! Four questions and one statement, each answered in a single line. There is
//! nothing to stream, because nothing the CLI can ask takes longer than a file
//! read — the daemon is never asked to *start* work, only what it already
//! knows, and once ([`Request::Invalidate`]) told that what it knows is wrong.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use snout_common::docs::{ErrorDoc, MovedDoc, UpdatesDoc};
use snout_common::fail::{Code, Context, Error, Result};
use snout_common::proto::{self, Reply, Request, socket_path};

use crate::daemon::Daemon;

/// The length of `struct sockaddr_un`'s `sun_path` on Linux, including the NUL
/// terminator.
const MAX_SOCKET_PATH: usize = 108;

/// How long a connection may stay silent before it is dropped.
///
/// A client that connects and never writes would otherwise hold a handler task
/// open through shutdown, and there is no legitimate reason to take longer than
/// this to send one line of JSON.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens the control socket.
///
/// A socket file left behind by a daemon that was killed would otherwise make
/// bind fail forever, so one that nothing is listening on is removed rather
/// than treated as a conflict. A live one is: two daemons sharing a cache would
/// fight over the same lock and the same scratch directory.
pub fn listen() -> Result<UnixListener> {
    bind_at(&socket_path())
}

pub fn bind_at(path: &Path) -> Result<UnixListener> {
    // The kernel's sun_path is 108 bytes including its terminator, and bind
    // reports a path that overflows it as a bare EINVAL — which is a miserable
    // thing to diagnose. Say what is actually wrong instead.
    let len = path.as_os_str().len();
    if len >= MAX_SOCKET_PATH {
        return Err(Error::config(format!(
            "socket path {} is {len} bytes; the kernel limit is {} — set XDG_RUNTIME_DIR to something shorter",
            path.display(),
            MAX_SOCKET_PATH - 1
        )));
    }

    // The socket controls this user's checks and nothing else, so it lives in a
    // directory nobody else can traverse. Confining it here rather than by
    // narrowing the socket's own mode after bind is what closes the window a
    // chmod-afterwards would leave open — and, unlike setting the umask around
    // bind, it is not a process-global side effect that other threads would see.
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(Code::Internal, || format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .with_context(Code::Internal, || format!("securing {}", dir.display()))?;

    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(Error::precondition(format!(
                "another snout daemon is already listening on {}",
                path.display()
            )));
        }
        let _ = std::fs::remove_file(path);
    }

    let listener = UnixListener::bind(path).with_context(Code::Internal, || {
        format!("listening on {}", path.display())
    })?;
    // Belt and braces behind the directory: a socket that is only reachable
    // through a 0700 directory is already private, but saying so costs nothing.
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    Ok(listener)
}

/// Accepts connections until cancelled, then waits for the handlers in flight.
pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener, cancel: CancellationToken) {
    let path = listener
        .local_addr()
        .ok()
        .and_then(|a| a.as_pathname().map(PathBuf::from));
    let mut handlers = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((conn, _)) => {
                    handlers.spawn(handle(Arc::clone(&daemon), conn, cancel.clone()));
                }
                Err(e) => daemon.log.error(format!("accept: {e}")),
            },
        }
    }

    drop(listener);
    while handlers.join_next().await.is_some() {}

    // Go's listener unlinks on close; tokio's does not, and a file left behind
    // makes the next daemon's bind look like a conflict.
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

async fn handle(daemon: Arc<Daemon>, conn: UnixStream, cancel: CancellationToken) {
    let (read_half, mut write) = conn.into_split();
    let mut reader = BufReader::new(read_half);

    // Two independent ways out, because either alone leaves a hole: the timeout
    // caps a client that connects and then idles while the daemon keeps
    // running, and the cancellation is what lets shutdown be immediate rather
    // than waiting out that timeout on every open connection.
    let read = tokio::select! {
        r = tokio::time::timeout(REQUEST_TIMEOUT, proto::read_line::<Request, _>(&mut reader)) => r,
        _ = cancel.cancelled() => return,
    };

    let reply = match read {
        // A bare connect-and-close is how a client probes for a daemon, so an
        // empty read is a normal event and not worth answering or logging.
        Ok(Ok(None)) | Err(_) => return,
        Ok(Ok(Some(req))) => answer(&daemon, req),
        Ok(Err(e)) => Reply::Error(ErrorDoc {
            code: Code::Usage,
            message: e.to_string(),
        }),
    };

    // A write that fails means the client hung up, which is not worth
    // reporting: nothing the daemon did depended on the reply arriving.
    let _ = proto::write_line(&mut write, &reply).await;
}

/// The whole request table.
///
/// Every arm is a read of state the daemon already holds, except `Invalidate`,
/// which is a write that finishes in microseconds. Nothing here can block, so
/// no handler can hold up a shutdown.
fn answer(daemon: &Daemon, req: Request) -> Reply {
    match req {
        Request::Status => Reply::Status(Box::new(daemon.status())),

        // The cache is read through the same generation guard `status` uses: an
        // answer about a system that has been switched away from is not stale,
        // it is wrong, and there is no honest way to show it. An answer that
        // fails that guard is reported as no answer at all, which is exactly
        // what a cold cache reports — a consumer has one empty case, not two.
        Request::ListUpdates => {
            Reply::Updates(UpdatesDoc::from(daemon.store.read_full().filter(|f| {
                daemon
                    .current_system()
                    .is_some_and(|c| f.current_system == c)
            })))
        }

        Request::ListMoved => Reply::Moved(MovedDoc::from(daemon.store.read_quick().filter(|q| {
            daemon
                .current_system()
                .is_some_and(|c| q.current_system == c)
        }))),

        Request::Invalidate => match daemon.invalidate() {
            Ok(()) => Reply::Ok,
            Err(e) => Reply::Error(ErrorDoc::from(&e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snout_common::config::Config;
    use snout_common::docs::{Full, Quick};
    use snout_common::nix::Summary;
    use snout_common::run::Runner;
    use snout_common::time::now;
    use tokio::io::AsyncWriteExt;

    use crate::log::Logger;
    use crate::store::Store;

    const RUNNING: &str = "/nix/store/aaa-system";
    const OLD: &str = "/nix/store/bbb-old-system";

    struct Served {
        _dir: tempfile::TempDir,
        path: PathBuf,
        daemon: Arc<Daemon>,
        cancel: CancellationToken,
        task: Option<tokio::task::JoinHandle<()>>,
    }

    async fn serve_one() -> Served {
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

        let daemon = Arc::new(Daemon::with_parts(
            cfg,
            Runner::fake(),
            Arc::new(store),
            Logger::discard(),
            Arc::new(now),
        ));

        let path = dir.path().join("s.sock");
        let listener = bind_at(&path).unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve(Arc::clone(&daemon), listener, cancel.clone()));

        Served {
            _dir: dir,
            path,
            daemon,
            cancel,
            task: Some(task),
        }
    }

    impl Served {
        /// One round trip, the way the CLI's client does it.
        async fn call(&self, req: Request) -> Reply {
            let conn = UnixStream::connect(&self.path).await.unwrap();
            let (r, mut w) = conn.into_split();
            proto::write_line(&mut w, &req).await.unwrap();
            let mut reader = BufReader::new(r);
            proto::read_line::<Reply, _>(&mut reader)
                .await
                .unwrap()
                .expect("the daemon closed without answering")
        }

        fn write_quick(&self, system: &str) {
            let q = Quick {
                schema_version: 1,
                checked_at: now(),
                available: true,
                moved_count: 2,
                moved: vec![],
                lock_hash: "hash".into(),
                current_system: system.into(),
                host: "somehost".into(),
                flake: "/etc/nixos".into(),
            };
            let s = &self.daemon.store;
            s.publish(&s.quick_path(), &q, "").unwrap();
        }

        fn write_full(&self, system: &str) {
            let f = Full {
                schema_version: 1,
                checked_at: now(),
                lock_hash: "hash".into(),
                current_system: system.into(),
                host: "somehost".into(),
                flake: "/etc/nixos".into(),
                summary: Summary {
                    count: 35,
                    ..Default::default()
                },
                updates: vec![],
            };
            let s = &self.daemon.store;
            s.publish(&s.full_path(), &f, "").unwrap();
        }

        /// Takes `&mut self` so the scratch directory outlives the shutdown —
        /// a test that inspects the socket path afterwards needs it to still
        /// exist.
        async fn shutdown(&mut self) {
            self.cancel.cancel();
            if let Some(task) = self.task.take() {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .expect("the server did not shut down")
                    .unwrap();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_status_request_round_trips() {
        let mut s = serve_one().await;
        match s.call(Request::Status).await {
            Reply::Status(st) => {
                assert!(st.daemon.running);
                assert_eq!(st.host, "somehost");
                assert!(!st.version.is_empty());
            }
            other => panic!("{other:?}"),
        }
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cold_cache_answers_with_an_empty_document_not_an_error() {
        // "Nothing checked yet" is a state worth showing, and the CLI should
        // not have to tell it apart from "nothing has changed".
        let mut s = serve_one().await;
        assert_eq!(
            s.call(Request::ListUpdates).await,
            Reply::Updates(UpdatesDoc::empty())
        );
        assert_eq!(
            s.call(Request::ListMoved).await,
            Reply::Moved(MovedDoc::empty())
        );
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_warm_cache_is_served_verbatim() {
        let mut s = serve_one().await;
        s.write_quick(RUNNING);
        s.write_full(RUNNING);

        match s.call(Request::ListUpdates).await {
            Reply::Updates(d) => {
                assert_eq!(d.summary.count, 35);
                assert!(d.checked_at.is_some());
            }
            other => panic!("{other:?}"),
        }
        match s.call(Request::ListMoved).await {
            Reply::Moved(d) => {
                assert_eq!(d.moved_count, 2);
                assert!(d.available);
            }
            other => panic!("{other:?}"),
        }
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_about_another_generation_is_served_as_no_answer() {
        // A package list for a system already switched away from is not stale,
        // it is wrong — and rendering it would read as "35 to update".
        let mut s = serve_one().await;
        s.write_quick(OLD);
        s.write_full(OLD);

        assert_eq!(
            s.call(Request::ListUpdates).await,
            Reply::Updates(UpdatesDoc::empty())
        );
        assert_eq!(
            s.call(Request::ListMoved).await,
            Reply::Moved(MovedDoc::empty())
        );
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalidate_empties_the_cache_and_says_so() {
        let mut s = serve_one().await;
        s.write_quick(RUNNING);
        s.write_full(RUNNING);

        assert_eq!(s.call(Request::Invalidate).await, Reply::Ok);
        assert_eq!(
            s.call(Request::ListUpdates).await,
            Reply::Updates(UpdatesDoc::empty())
        );
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unintelligible_request_is_answered_rather_than_ignored() {
        // Silence would leave the client blocked until its own read gave up.
        let mut s = serve_one().await;
        let conn = UnixStream::connect(&s.path).await.unwrap();
        let (r, mut w) = conn.into_split();
        w.write_all(b"{\"method\":\"sudo\"}\n").await.unwrap();

        let mut reader = BufReader::new(r);
        match proto::read_line::<Reply, _>(&mut reader).await.unwrap() {
            Some(Reply::Error(e)) => assert_eq!(e.code, Code::Usage),
            other => panic!("{other:?}"),
        }
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_client_cannot_hold_shutdown_open() {
        // A connection that never writes would otherwise pin a handler task,
        // and the graceful path would wait on it forever.
        let mut s = serve_one().await;
        let _idle = UnixStream::connect(&s.path).await.unwrap();
        // Shutdown must complete well inside the request timeout's 10s, which
        // `shutdown` enforces with a 5s cap of its own.
        s.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutting_down_removes_the_socket_file() {
        // A file left behind makes the next daemon's bind look like a conflict.
        let mut s = serve_one().await;
        let path = s.path.clone();
        assert!(path.exists());
        s.shutdown().await;
        assert!(!path.exists(), "socket file survived shutdown");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_socket_is_a_conflict_and_a_dead_one_is_replaced() {
        let mut s = serve_one().await;
        let path = s.path.clone();
        // Live: a second daemon must not share the cache.
        let err = bind_at(&path).unwrap_err();
        assert!(err.to_string().contains("already listening"), "{err}");
        s.shutdown().await;

        // Dead: a leftover file must not block startup forever.
        std::fs::write(&path, "").unwrap();
        let l = bind_at(&path).unwrap();
        drop(l);
    }

    #[test]
    fn an_over_long_socket_path_says_what_is_actually_wrong() {
        // bind reports this as a bare EINVAL, which is a miserable thing to
        // diagnose.
        let path = PathBuf::from(format!("/tmp/{}/snout.sock", "x".repeat(120)));
        let err = bind_at(&path).unwrap_err();
        assert!(err.to_string().contains("kernel limit is 107"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_is_reachable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let mut s = serve_one().await;
        let mode = std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket mode was {mode:o}");
        s.shutdown().await;
    }
}
