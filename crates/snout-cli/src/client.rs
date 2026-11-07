//! Talking to the daemon.
//!
//! Every command goes through here first, including the three that then do
//! their work locally. That is the rule the split rests on: the daemon owns the
//! update cache, so `snout` running without one is a tool answering from
//! nothing, and a build started in that state would leave the cache describing
//! a system that no longer exists with nobody to correct it.
//!
//! So "no daemon" is a failure, not a fallback. There is exactly one exception,
//! `status`, whose entire job is to report on the daemon — and telling you it
//! is not running is a useful answer rather than a refusal to give one.

use std::path::{Path, PathBuf};

use tokio::io::BufReader;
use tokio::net::UnixStream;

use snout_common::DAEMON_PROG;
use snout_common::docs::{MovedDoc, Status, UpdatesDoc};
use snout_common::fail::{Code, Error, Result};
use snout_common::proto::{self, DIAL_TIMEOUT, Reply, Request};

pub struct Client {
    pub path: PathBuf,
}

impl Client {
    pub fn new(path: impl Into<PathBuf>) -> Client {
        Client { path: path.into() }
    }

    /// Confirms there is a daemon to talk to, and says so plainly if not.
    ///
    /// Called by the commands that do their work locally: they need nothing
    /// *from* the daemon, but they need it to exist, because each of them ends
    /// by telling it the world has changed.
    pub async fn require(&self) -> Result<()> {
        match connect(&self.path).await {
            Ok(_) => Ok(()),
            Err(e) => {
                proto::clear_stale(&self.path, &e);
                Err(unavailable())
            }
        }
    }

    /// One round trip: connect, send, read one reply.
    async fn call(&self, req: &Request) -> Result<Reply> {
        let conn = match connect(&self.path).await {
            Ok(c) => c,
            Err(e) => {
                proto::clear_stale(&self.path, &e);
                return Err(unavailable());
            }
        };

        let (read_half, mut write) = conn.into_split();
        proto::write_line(&mut write, req).await?;

        let mut reader = BufReader::new(read_half);
        proto::read_line(&mut reader)
            .await?
            // The daemon closing without answering is not the same as it not
            // being there: something went wrong mid-request, and reporting it
            // as "not running" would send the reader looking in the wrong
            // place.
            .ok_or_else(|| Error::internal("the daemon closed without answering"))
    }

    pub async fn status(&self) -> Result<Status> {
        match self.call(&Request::Status).await? {
            Reply::Status(st) => Ok(*st),
            other => Err(other.into_error()),
        }
    }

    pub async fn list_updates(&self) -> Result<UpdatesDoc> {
        match self.call(&Request::ListUpdates).await? {
            Reply::Updates(d) => Ok(d),
            other => Err(other.into_error()),
        }
    }

    pub async fn list_moved(&self) -> Result<MovedDoc> {
        match self.call(&Request::ListMoved).await? {
            Reply::Moved(d) => Ok(d),
            other => Err(other.into_error()),
        }
    }

    /// Tells the daemon its cache describes a world that has stopped existing.
    pub async fn invalidate(&self) -> Result<()> {
        match self.call(&Request::Invalidate).await? {
            Reply::Ok => Ok(()),
            other => Err(other.into_error()),
        }
    }
}

/// Exit 3, "precondition not met" — the daemon not running is a state of the
/// machine, not a fault in what was typed.
fn unavailable() -> Error {
    Error::new(
        Code::Precondition,
        format!(
            "{DAEMON_PROG} is not running — start it with 'systemctl --user start {DAEMON_PROG}'"
        ),
    )
}

async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    match tokio::time::timeout(DIAL_TIMEOUT, UnixStream::connect(path)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "dial timed out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_socket_at_all_is_a_precondition_naming_the_daemon() {
        // The reader has to be told which of the two halves is missing, and
        // how to start it — "connection refused" is not that.
        let dir = tempfile::tempdir().unwrap();
        let c = Client::new(dir.path().join("absent.sock"));

        let err = c.status().await.unwrap_err();
        assert_eq!(err.code, Code::Precondition);
        assert_eq!(err.exit_code(), snout_common::fail::EXIT_PRECONDITION);
        assert!(err.to_string().contains(DAEMON_PROG), "{err}");
        assert!(err.to_string().contains("systemctl --user"), "{err}");
    }

    #[tokio::test]
    async fn a_socket_nobody_is_listening_on_is_removed() {
        // So the next daemon to start is not blocked by a file its predecessor
        // left behind when it was killed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead.sock");
        // Binding and dropping leaves the file with no listener behind it.
        {
            let _l = tokio::net::UnixListener::bind(&path).unwrap();
        }
        assert!(path.exists());

        assert!(Client::new(&path).require().await.is_err());
        assert!(!path.exists(), "stale socket was left in place");
    }

    #[tokio::test]
    async fn a_daemon_that_hangs_up_mid_request_is_not_reported_as_absent() {
        // Something went wrong inside it; saying "not running" would send the
        // reader off to start a daemon that is already there.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rude.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            // Take the request, then hang up without answering it.
            if let Ok((conn, _)) = listener.accept().await {
                let mut r = BufReader::new(conn);
                let _ = proto::read_line::<serde_json::Value, _>(&mut r).await;
            }
        });

        let err = Client::new(&path).status().await.unwrap_err();
        assert_eq!(err.code, Code::Internal);
        assert!(
            err.to_string().contains("closed without answering"),
            "{err}"
        );
    }
}
