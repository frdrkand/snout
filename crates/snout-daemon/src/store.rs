//! The cache directory: two published answers, the daemon's private
//! bookkeeping, the scratch copy of the flake, and the lock that serialises
//! runs.
//!
//! Two rules govern everything here:
//!
//! - A failed check never overwrites a good cache file. Losing a
//!   last-known-good answer because the network was down is worse than showing
//!   one a few hours stale, and both surfaces report when they last succeeded.
//!
//! - A check that *succeeded* can still have its answer thrown away — see
//!   [`Store::publish`] — when the system it measured stopped running while it
//!   ran.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use snout_common::docs::{CURRENT_SYSTEM_LINK, Full, Quick};
use snout_common::fail::{Code, Context, Error, Result};
use snout_common::time::Timestamp;

/// Resolves the running system's store path.
///
/// Resolved, unlike the callers that hand the link straight to `nix path-info`
/// and let nix resolve it: this one is an identity a cached answer is stamped
/// with, and the link moves under it on every deploy.
pub fn current_system() -> std::io::Result<String> {
    std::fs::canonicalize(CURRENT_SYSTEM_LINK).map(|p| p.to_string_lossy().into_owned())
}

/// Resolves the running system. Injectable so a test can move the system out
/// from under a check.
pub type SystemProbe = Arc<dyn Fn() -> std::io::Result<String> + Send + Sync>;

pub struct Store {
    pub dir: PathBuf,
    pub current_system: SystemProbe,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store {
            dir: dir.into(),
            current_system: Arc::new(current_system),
        }
    }

    pub fn quick_path(&self) -> PathBuf {
        self.dir.join("quick.json")
    }
    pub fn full_path(&self) -> PathBuf {
        self.dir.join("updates.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }
    /// The scratch copy of the flake both tiers work in.
    pub fn work_path(&self) -> PathBuf {
        self.dir.join("flake")
    }
    pub fn failed_path(&self) -> PathBuf {
        self.dir.join("full.failed")
    }
    /// The built toplevel, kept as an indirect GC root so the weekly `nix.gc`
    /// does not throw away what was just downloaded. The escalated check
    /// doubles as a prefetch, which is what makes the eventual update near
    /// instant.
    pub fn result_link(&self) -> PathBuf {
        self.dir.join("result")
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).context(Code::Internal, "creating cache directory")
    }

    /// The cached quick answer, or `None` if there is not one. A corrupt or
    /// half-written file reads as absent rather than as an error: the next
    /// check replaces it, and until then "never checked" is the honest state.
    pub fn read_quick(&self) -> Option<Quick> {
        read_json(&self.quick_path())
    }

    pub fn read_full(&self) -> Option<Full> {
        read_json(&self.full_path())
    }

    /// Writes a document, but only if the system it describes is still the one
    /// running.
    ///
    /// A deploy clears this cache on purpose when it switches, and a check that
    /// started before that must not put the old answer back a minute later — it
    /// would look current, and be a description of the previous generation.
    pub fn publish<T: Serialize>(&self, path: &Path, v: &T, system_at_start: &str) -> Result<()> {
        match (self.current_system)() {
            Ok(now) if !system_at_start.is_empty() && now != system_at_start => {
                // Not an error: the answer was correct, it is simply about a
                // system that has stopped running.
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                // The guard cannot be evaluated, so it cannot be honoured.
                // Refusing to publish would mean a machine with no
                // /run/current-system never caches anything at all, and the
                // check that produced this answer already required the probe to
                // succeed — so reaching here means it broke mid-flight.
                return Err(Error::wrap(
                    Code::Precondition,
                    e,
                    format!("reading {CURRENT_SYSTEM_LINK}"),
                ));
            }
        }
        write_json_atomic(path, v)
    }

    /// Removes the published answers and the prefetched closure, which is what
    /// a deploy does the moment it has switched: both answers are then about a
    /// system that has stopped running.
    ///
    /// Deliberately leaves the lock and the scratch copy alone — a concurrent
    /// check is holding one and reading the other. No lock is taken here
    /// either: waiting behind an in-flight build would stall the tail of a
    /// deploy for minutes.
    ///
    /// The daemon's failure bookkeeping goes too. Leaving it would let `status`
    /// report a failing check with no cached answers at all, and keep a backoff
    /// running against a question nobody has asked yet.
    pub fn clear(&self) -> Result<()> {
        let mut first: Option<Error> = None;
        for p in [
            self.quick_path(),
            self.full_path(),
            self.result_link(),
            self.failed_path(),
            self.state_path(),
            // Staging files a crash mid-write may have left behind. Nothing
            // else ever removes them.
            self.quick_path().with_extension("json.new"),
            self.full_path().with_extension("json.new"),
            self.state_path().with_extension("json.new"),
        ] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if first.is_none() => {
                    first = Some(Error::wrap(
                        Code::Internal,
                        e,
                        format!("removing {}", p.display()),
                    ));
                }
                Err(_) => {}
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The lock hash of the last build that failed, if any.
    ///
    /// Keyed on the lock rather than being a flag, so upstream moving again is
    /// retried by itself: a lock that has moved has a different hash, and the
    /// marker stops matching.
    pub fn failed_lock(&self) -> String {
        std::fs::read_to_string(self.failed_path())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    pub fn set_failed_lock(&self, hash: &str) {
        let _ = std::fs::write(self.failed_path(), format!("{hash}\n"));
    }

    pub fn clear_failed_lock(&self) {
        let _ = std::fs::remove_file(self.failed_path());
    }

    pub fn read_state(&self) -> State {
        read_json(&self.state_path()).unwrap_or_default()
    }

    pub fn write_state(&self, st: &State) -> Result<()> {
        write_json_atomic(&self.state_path(), st)
    }

    /// Takes the lock that serialises checks against each other.
    ///
    /// Both tiers wipe and rebuild the same scratch copy, so without this a
    /// timer-driven check and one typed in a terminal would pull the directory
    /// out from under each other. It waits rather than failing: a check nobody
    /// asked for is worth a few seconds' delay, and an error the user did not
    /// cause is worse than one.
    ///
    /// `on_wait` fires only if the lock is actually contended, so the common
    /// case does not flash "waiting" at a surface for a frame.
    pub fn acquire(&self, on_wait: impl FnOnce()) -> Result<Lock> {
        use rustix::fs::{FlockOperation, flock};

        let f = File::options()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())
            .context(Code::Internal, "opening lock")?;

        match flock(&f, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                on_wait();
                flock(&f, FlockOperation::LockExclusive).context(Code::Internal, "locking")?;
            }
            Err(e) => return Err(Error::wrap(Code::Internal, e, "locking")),
        }
        Ok(Lock { file: Some(f) })
    }
}

/// Held for the whole of a check, including the multi-minute build.
pub struct Lock {
    file: Option<File>,
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = rustix::fs::flock(&f, rustix::fs::FlockOperation::Unlock);
        }
    }
}

/// The daemon's private bookkeeping.
///
/// Kept out of the two published documents so those stay pure answers, with
/// nothing operational mixed in for a consumer to trip over.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<Timestamp>,
    /// What the most recent failed check said, kept so a surface polling
    /// `status` can report that checks are failing. Cleared on the next
    /// success, alongside `failures`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// That failure's contract code, so a consumer can branch on it without
    /// matching on message text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<snout_common::fail::Code>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let b = std::fs::read(path).ok()?;
    serde_json::from_slice(&b).ok()
}

/// Writes a document through a sibling `.new` file and renames it into place,
/// so a crash mid-write cannot leave a reader looking at a truncated cache. A
/// failed rename takes the temporary file with it rather than leaving litter
/// behind for the next run to wonder about.
fn write_json_atomic<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let mut b =
        serde_json::to_vec_pretty(v).map_err(|e| Error::wrap(Code::Internal, e, "encoding"))?;
    b.push(b'\n');

    let tmp = path.with_extension(format!(
        "{}.new",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, &b)
        .with_context(Code::Internal, || format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::wrap(
            Code::Internal,
            e,
            format!("writing {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use snout_common::time::now;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::new(dir.path().join("updates"));
        s.current_system = Arc::new(|| Ok("/nix/store/aaa-system".into()));
        s.ensure().unwrap();
        (dir, s)
    }

    fn quick(hash: &str, system: &str) -> Quick {
        Quick {
            schema_version: 1,
            checked_at: now(),
            available: true,
            moved_count: 1,
            moved: vec![],
            lock_hash: hash.into(),
            current_system: system.into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
        }
    }

    #[test]
    fn an_absent_cache_reads_as_nothing_checked() {
        let (_d, s) = store();
        assert!(s.read_quick().is_none());
        assert!(s.read_full().is_none());
        assert_eq!(s.read_state(), State::default());
    }

    #[test]
    fn a_corrupt_cache_reads_as_absent_rather_than_failing() {
        // The next check replaces it, and until then "never checked" is the
        // honest state. An error here would take down a `status` call that has
        // nothing to do with the corruption.
        let (_d, s) = store();
        std::fs::write(s.quick_path(), "{ truncated").unwrap();
        assert!(s.read_quick().is_none());
    }

    #[test]
    fn a_published_answer_reads_back() {
        let (_d, s) = store();
        let q = quick("abc", "/nix/store/aaa-system");
        s.publish(&s.quick_path(), &q, "/nix/store/aaa-system")
            .unwrap();
        assert_eq!(s.read_quick().unwrap(), q);
    }

    #[test]
    fn an_answer_about_a_system_that_stopped_running_is_discarded() {
        // The deploy that switched cleared this cache on purpose. A check that
        // started beforehand must not put the old answer back a minute later:
        // it would look current and describe the previous generation.
        let (_d, s) = store();
        let q = quick("abc", "/nix/store/old-system");
        s.publish(&s.quick_path(), &q, "/nix/store/old-system")
            .unwrap();
        assert!(s.read_quick().is_none(), "stale answer was written anyway");
    }

    #[test]
    fn publishing_without_a_system_stamp_is_never_discarded() {
        // An empty stamp means the caller is not making a claim about which
        // generation the document describes — the daemon's own state file.
        let (_d, s) = store();
        s.publish(&s.state_path(), &State::default(), "").unwrap();
        assert!(s.state_path().exists());
    }

    #[test]
    fn a_write_leaves_no_staging_file_behind() {
        let (_d, s) = store();
        s.publish(&s.quick_path(), &quick("a", "/nix/store/aaa-system"), "")
            .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&s.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".new"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn clear_removes_the_answers_and_the_failure_bookkeeping() {
        let (_d, s) = store();
        s.publish(&s.quick_path(), &quick("a", "/nix/store/aaa-system"), "")
            .unwrap();
        s.set_failed_lock("abc");
        s.write_state(&State {
            failures: 3,
            last_error: Some("network went away".into()),
            ..Default::default()
        })
        .unwrap();
        std::fs::write(s.result_link(), "").unwrap();

        s.clear().unwrap();

        assert!(s.read_quick().is_none());
        assert_eq!(s.failed_lock(), "");
        assert!(!s.result_link().exists());
        // Without this, `status` reports a failing check with no cached answers
        // at all, and a backoff keeps running against a question nobody asked.
        assert_eq!(s.read_state(), State::default());
    }

    #[test]
    fn clear_leaves_the_lock_and_the_scratch_copy_alone() {
        // A concurrent check is holding one and reading the other.
        let (_d, s) = store();
        let _lock = s.acquire(|| panic!("should not have waited")).unwrap();
        std::fs::create_dir_all(s.work_path()).unwrap();
        std::fs::write(s.work_path().join("flake.nix"), "{}").unwrap();

        s.clear().unwrap();

        assert!(s.lock_path().exists());
        assert!(s.work_path().join("flake.nix").exists());
    }

    #[test]
    fn clear_on_an_empty_cache_is_not_an_error() {
        let (_d, s) = store();
        s.clear().unwrap();
    }

    #[test]
    fn the_failed_lock_is_a_hash_not_a_flag() {
        // Keyed on the lock so upstream moving again retries by itself.
        let (_d, s) = store();
        assert_eq!(s.failed_lock(), "");
        s.set_failed_lock("deadbeef");
        assert_eq!(s.failed_lock(), "deadbeef");
        s.clear_failed_lock();
        assert_eq!(s.failed_lock(), "");
    }

    #[test]
    fn state_round_trips_and_omits_what_is_unset() {
        let (_d, s) = store();
        let st = State {
            failures: 2,
            last_error: Some("boom".into()),
            last_error_code: Some(snout_common::fail::Code::Network),
            ..Default::default()
        };
        s.write_state(&st).unwrap();
        assert_eq!(s.read_state(), st);

        let raw = std::fs::read_to_string(s.state_path()).unwrap();
        assert!(!raw.contains("lastCheckedAt"), "{raw}");
        assert!(raw.contains("\"lastErrorCode\": \"network\""), "{raw}");
    }

    #[test]
    fn the_lock_is_exclusive_across_processes() {
        // flock is per open file description, so two Store handles on one path
        // model two processes well enough to prove exclusion.
        let (_d, s) = store();
        let held = s.acquire(|| panic!("uncontended")).unwrap();

        let mut waited = false;
        // Non-blocking probe: taking it for real would deadlock this thread.
        let f = File::options()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(s.lock_path())
            .unwrap();
        if rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
            waited = true;
        }
        assert!(waited, "a second holder got the lock while it was held");

        drop(held);
        // Once released it is available again, with no wait callback.
        let _ = s.acquire(|| panic!("should not have waited")).unwrap();
    }

    #[test]
    fn an_uncontended_lock_never_reports_waiting() {
        // Otherwise every check flashes "Waiting for another check" for a frame.
        let (_d, s) = store();
        let _l = s
            .acquire(|| panic!("reported a wait that did not happen"))
            .unwrap();
    }
}
