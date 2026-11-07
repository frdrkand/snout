//! The protocol between `snout` and `snout-daemon`.
//!
//! A unix socket under `$XDG_RUNTIME_DIR` carrying newline-delimited JSON. One
//! request line, one reply line, connection closes. The daemon's only clients
//! are the CLI and anything a user scripts, and neither asks it to do work that
//! takes longer than a file read — so there is nothing to stream, and no
//! sequencing to get wrong.
//!
//! What crosses it is deliberately small: everything the CLI can ask is a
//! question about what the daemon already knows, plus one statement of fact
//! ([`Request::Invalidate`]) about a system that has just changed underneath it,
//! and one request ([`Request::Check`]) to run the schedule's check now. Neither
//! of the last two waits for the check: the reply says it was arranged, and
//! `status` is how anyone follows it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::docs::{ErrorDoc, MovedDoc, Status, UpdatesDoc};
use crate::fail::{Code, Context, Error, Result};

/// Short on purpose: this is a local socket, and a CLI that hangs for seconds
/// deciding whether a daemon exists is worse than one that says so at once.
pub const DIAL_TIMEOUT: Duration = Duration::from_millis(500);

/// One line from client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Request {
    /// The snapshot: the daemon's version and schedule, and what the cache
    /// holds.
    Status,
    /// The cached package diff.
    ListUpdates,
    /// The cached moved-inputs answer.
    ListMoved,
    /// "The system changed" — drop the cache and check again now.
    ///
    /// Sent by the CLI after a deploy or a lock rewrite. Both leave the cached
    /// answers describing a world that has stopped existing, and waiting out a
    /// whole interval would advertise an update that is already installed.
    Invalidate,
    /// "Check now" — the scheduled check, brought forward, keeping the cache.
    ///
    /// Unlike [`Request::Invalidate`] nothing is dropped: the answers on disk
    /// are still true until the new check replaces them. It also forgets that
    /// the current lock failed to build, because asking by hand is exactly a
    /// request to try again.
    Check,
}

/// One line from server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Reply {
    Status(Box<Status>),
    Updates(UpdatesDoc),
    Moved(MovedDoc),
    /// The answer to [`Request::Check`]. `started` is false when a check was
    /// already running, in which case that one is the answer and no second
    /// one is queued behind it.
    Check {
        started: bool,
    },
    Ok,
    Error(ErrorDoc),
}

impl Reply {
    /// Turns a reported failure back into an error, preserving the contract
    /// code so the exit status is the same wherever the failure happened.
    pub fn into_error(self) -> Error {
        match self {
            Reply::Error(d) => d.into(),
            other => Error::internal(format!(
                "the daemon answered a different question: {}",
                serde_json::to_string(&other).unwrap_or_default()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Where it lives
// ---------------------------------------------------------------------------

/// Where the daemon's socket lives. `$XDG_RUNTIME_DIR` is a tmpfs cleared
/// between logins, which is the right lifetime for a socket.
fn runtime_dir() -> PathBuf {
    let non_empty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(d) = non_empty("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("snout");
    }
    let tmp = non_empty("TMPDIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(tmp).join(format!("snout-{}", rustix::process::getuid().as_raw()))
}

/// The daemon's control socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("snout.sock")
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Writes one JSON document and its newline.
pub async fn write_line<T, W>(w: &mut W, v: &T) -> Result<()>
where
    T: Serialize,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut bytes =
        serde_json::to_vec(v).map_err(|e| Error::wrap(Code::Internal, e, "encoding"))?;
    bytes.push(b'\n');
    w.write_all(&bytes)
        .await
        .context(Code::Internal, "writing to the socket")
}

/// Reads one JSON document, or `None` if the peer closed without sending one.
///
/// The whole line, however long: nix emits very long lines, and a fixed-size
/// buffer would split one and turn a message into a parse error.
pub async fn read_line<T, R>(r: &mut BufReader<R>) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = String::new();
    let n = r
        .read_line(&mut buf)
        .await
        .context(Code::Internal, "reading from the socket")?;
    if n == 0 {
        return Ok(None);
    }
    serde_json::from_str(buf.trim_end())
        .map(Some)
        .map_err(|e| Error::wrap(Code::Internal, e, "unintelligible message"))
}

/// Removes a socket file that nothing is listening on.
///
/// Only on `ECONNREFUSED`, which is the one error that actually means "the path
/// exists and nobody is behind it". A timeout must not qualify: a busy daemon
/// is exactly when a 500 ms dial expires, and unlinking its socket then would
/// leave a healthy daemon unreachable until it restarted.
pub fn clear_stale(path: &Path, err: &std::io::Error) {
    if err.kind() != std::io::ErrorKind::ConnectionRefused {
        return;
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::SCHEMA_VERSION;
    use crate::testenv::env_guard as guard;

    #[test]
    fn a_request_is_tagged_by_method() {
        assert_eq!(
            serde_json::to_value(Request::Status).unwrap(),
            serde_json::json!({"method": "status"})
        );
        assert_eq!(
            serde_json::to_value(Request::ListUpdates).unwrap(),
            serde_json::json!({"method": "list-updates"})
        );
        assert_eq!(
            serde_json::to_value(Request::Invalidate).unwrap(),
            serde_json::json!({"method": "invalidate"})
        );
        assert_eq!(
            serde_json::to_value(Request::Check).unwrap(),
            serde_json::json!({"method": "check"})
        );
    }

    #[test]
    fn a_check_reply_says_whether_it_started_one() {
        assert_eq!(
            serde_json::to_value(Reply::Check { started: true }).unwrap(),
            serde_json::json!({"type": "check", "started": true})
        );
    }

    #[test]
    fn an_unknown_method_is_a_parse_error_not_a_silent_status() {
        assert!(serde_json::from_str::<Request>(r#"{"method":"sudo"}"#).is_err());
    }

    #[test]
    fn every_request_round_trips() {
        for r in [
            Request::Status,
            Request::ListUpdates,
            Request::ListMoved,
            Request::Invalidate,
            Request::Check,
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn a_reply_is_tagged_by_type_and_keeps_the_contract_key_names() {
        let v = serde_json::to_value(Reply::Updates(UpdatesDoc::empty())).unwrap();
        assert_eq!(v["type"], "updates");
        assert_eq!(v["schemaVersion"], 1);
        assert_eq!(v["updates"], serde_json::json!([]));
    }

    #[test]
    fn a_reported_failure_becomes_an_error_with_the_same_exit_code() {
        // The whole reason the code travels: `snout list-updates` must exit 3
        // for a precondition whether the daemon or the CLI hit it.
        let err = Reply::Error(ErrorDoc {
            code: Code::Precondition,
            message: "nothing checked yet".into(),
        })
        .into_error();
        assert_eq!(err.code, Code::Precondition);
        assert_eq!(err.exit_code(), crate::fail::EXIT_PRECONDITION);
    }

    #[test]
    fn an_answer_to_a_different_question_is_an_error_not_a_panic() {
        // A daemon a version out of step could send anything; the client must
        // not unwrap its way into a crash.
        let err = Reply::Ok.into_error();
        assert_eq!(err.code, Code::Internal);
        assert!(err.to_string().contains("different question"), "{err}");
    }

    #[tokio::test]
    async fn a_line_round_trips_through_the_framing() {
        let mut buf: Vec<u8> = Vec::new();
        write_line(&mut buf, &Request::ListMoved).await.unwrap();
        assert!(buf.ends_with(b"\n"), "no newline terminator");

        let mut r = BufReader::new(&buf[..]);
        let got: Option<Request> = read_line(&mut r).await.unwrap();
        assert_eq!(got, Some(Request::ListMoved));

        // A second read finds the stream ended rather than blocking or failing.
        let none: Option<Request> = read_line(&mut r).await.unwrap();
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn a_reply_longer_than_any_buffer_survives_the_round_trip() {
        // nix emits very long lines, and a fixed-size read would split one.
        let mut doc = UpdatesDoc::empty();
        doc.schema_version = SCHEMA_VERSION;
        doc.updates = (0..2000)
            .map(|i| crate::nix::Change {
                name: format!("package-with-a-long-name-number-{i}"),
                from: "1.0.0".into(),
                to: "2.0.0".into(),
                kind: crate::nix::Kind::Changed,
                size_delta: 0,
            })
            .collect();

        let mut buf: Vec<u8> = Vec::new();
        write_line(&mut buf, &Reply::Updates(doc.clone()))
            .await
            .unwrap();
        assert!(
            buf.len() > 64 * 1024,
            "the fixture is not long enough to test this"
        );

        let mut r = BufReader::new(&buf[..]);
        let got: Option<Reply> = read_line(&mut r).await.unwrap();
        assert_eq!(got, Some(Reply::Updates(doc)));
    }

    #[test]
    fn the_socket_lives_under_the_runtime_dir() {
        let _g = guard();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
        assert_eq!(socket_path(), dir.path().join("snout/snout.sock"));

        // Without a runtime dir it falls back to a uid-scoped temp path, so two
        // users on one machine cannot collide.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let p = socket_path();
        assert!(
            p.to_string_lossy()
                .contains(&format!("snout-{}", rustix::process::getuid().as_raw())),
            "{p:?}"
        );
    }

    #[test]
    fn a_timeout_never_unlinks_a_live_daemons_socket() {
        // A busy daemon is exactly when a 500 ms dial expires. Unlinking then
        // leaves a healthy daemon unreachable until it restarts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.sock");
        std::fs::write(&path, "").unwrap();

        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::PermissionDenied,
        ] {
            clear_stale(&path, &std::io::Error::new(kind, "x"));
            assert!(path.exists(), "{kind:?} removed a live socket");
        }

        clear_stale(
            &path,
            &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        assert!(!path.exists(), "a refused connection left a dead socket");
    }
}
