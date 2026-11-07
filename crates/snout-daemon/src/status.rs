//! Assembling the snapshot the CLI's `status` renders.
//!
//! Reading the cache, and nothing else. This never initiates work — that is
//! what makes it safe for a status bar to poll every ten seconds. A status call
//! that could start a ten-minute build would force every consumer to reason
//! about whether reading is expensive.

use snout_common::docs::{DaemonStatus, ErrorDoc, FullStatus, QuickStatus, SCHEMA_VERSION, Status};
use snout_common::version;

use crate::store::Store;

impl Store {
    /// Reads the cache and assembles a snapshot.
    ///
    /// `current` is the running system's store path. Answers stamped with any
    /// other generation are left out: a package count for a system you have
    /// already switched away from is not stale, it is wrong, and reporting it
    /// as current is worse than reporting nothing. `None` means the running
    /// system could not be determined, in which case nothing can be vouched for
    /// and neither answer is shown.
    ///
    /// The `daemon` block is left at its default here and filled in by the
    /// caller, which is the only thing that knows the schedule.
    pub fn snapshot(&self, host: &str, flake: &str, current: Option<&str>) -> Status {
        let mut st = Status {
            schema_version: SCHEMA_VERSION,
            version: version::short(),
            host: host.to_string(),
            flake: flake.to_string(),
            daemon: DaemonStatus::default(),
            quick: QuickStatus::default(),
            full: FullStatus::default(),
            error: None,
        };

        if let Some(current) = current {
            if let Some(q) = self.read_quick().filter(|q| q.current_system == current) {
                st.quick = QuickStatus {
                    checked_at: Some(q.checked_at),
                    available: q.available,
                    moved_count: q.moved_count,
                    moved: q.moved,
                };
            }
            if let Some(f) = self.read_full().filter(|f| f.current_system == current) {
                st.full = FullStatus {
                    checked_at: Some(f.checked_at),
                    summary: f.summary,
                };
            }
        }

        // Read from the daemon's own bookkeeping rather than from either
        // answer: a failed check deliberately never overwrites a good one, so
        // both documents above go on describing the last time things worked.
        let state = self.read_state();
        if let Some(message) = state.last_error {
            st.error = Some(ErrorDoc {
                code: state
                    .last_error_code
                    .unwrap_or(snout_common::fail::Code::Internal),
                message,
            });
        }

        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::State;
    use snout_common::docs::{Full, Quick};
    use snout_common::fail::Code;
    use snout_common::lockdiff::Moved;
    use snout_common::nix::Summary;
    use snout_common::time::now;
    use std::sync::Arc;

    const RUNNING: &str = "/nix/store/aaa-system";
    const OLD: &str = "/nix/store/bbb-old-system";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::new(dir.path().join("updates"));
        s.current_system = Arc::new(|| Ok(RUNNING.into()));
        s.ensure().unwrap();
        (dir, s)
    }

    fn write_quick(s: &Store, system: &str) {
        let q = Quick {
            schema_version: 1,
            checked_at: now(),
            available: true,
            moved_count: 2,
            moved: vec![Moved {
                name: "nixpkgs".into(),
                from: "aaa".into(),
                to: "bbb".into(),
            }],
            lock_hash: "hash".into(),
            current_system: system.into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
        };
        s.publish(&s.quick_path(), &q, "").unwrap();
    }

    fn write_full(s: &Store, system: &str) {
        let f = Full {
            schema_version: 1,
            checked_at: now(),
            lock_hash: "hash".into(),
            current_system: system.into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
            summary: Summary {
                count: 35,
                changed: 31,
                added: 1,
                removed: 3,
                size_only: 0,
                total_size_delta: 23_000_000,
            },
            updates: vec![],
        };
        s.publish(&s.full_path(), &f, "").unwrap();
    }

    #[test]
    fn an_empty_cache_reports_nothing_checked() {
        let (_d, s) = store();
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        assert!(st.quick.checked_at.is_none());
        assert!(st.full.checked_at.is_none());
        assert!(st.error.is_none());
        assert!(!st.daemon.running);
        // Never null on the wire: a consumer should be able to iterate it
        // without a null check.
        assert!(st.quick.moved.is_empty());
    }

    #[test]
    fn the_snapshot_names_the_version_that_produced_it() {
        // The CLI and the daemon are separate binaries; a half-upgraded machine
        // is exactly the state worth being able to see.
        let (_d, s) = store();
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        assert!(st.version.contains(snout_common::version::VERSION));
    }

    #[test]
    fn both_answers_are_reported_when_they_describe_the_running_system() {
        let (_d, s) = store();
        write_quick(&s, RUNNING);
        write_full(&s, RUNNING);
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        assert_eq!(st.quick.moved_count, 2);
        assert_eq!(st.full.summary.count, 35);
    }

    #[test]
    fn each_answer_is_judged_on_its_own_stamp() {
        // A deploy can leave a fresh quick answer beside a full one from before
        // the switch; showing the first and hiding the second is correct.
        let (_d, s) = store();
        write_quick(&s, RUNNING);
        write_full(&s, OLD);
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        assert!(st.quick.checked_at.is_some());
        assert!(st.full.checked_at.is_none());
    }

    #[test]
    fn nothing_is_vouched_for_when_the_running_system_is_unknown() {
        let (_d, s) = store();
        write_quick(&s, RUNNING);
        write_full(&s, RUNNING);
        let st = s.snapshot("somehost", "/etc/nixos", None);
        assert!(st.quick.checked_at.is_none());
        assert!(st.full.checked_at.is_none());
    }

    #[test]
    fn a_failing_check_is_reported_alongside_the_last_good_answer() {
        // A failed check never overwrites a good one, so without this a surface
        // sits on a week-old answer with no way to tell checks are broken.
        let (_d, s) = store();
        write_quick(&s, RUNNING);
        s.write_state(&State {
            failures: 2,
            last_error: Some("updating flake inputs: exit 1".into()),
            last_error_code: Some(Code::Network),
            ..Default::default()
        })
        .unwrap();

        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        assert!(st.quick.checked_at.is_some());
        let e = st.error.unwrap();
        assert_eq!(e.code, Code::Network);
        assert!(e.message.contains("updating flake inputs"));
    }

    #[test]
    fn the_snapshot_serialises_with_the_contract_field_names() {
        let (_d, s) = store();
        write_full(&s, RUNNING);
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        let v = serde_json::to_value(&st).unwrap();

        assert_eq!(v["schemaVersion"], 1);
        assert_eq!(v["daemon"]["running"], false);
        assert_eq!(v["daemon"]["busy"], false);
        assert!(v["daemon"]["nextCheckAt"].is_null());
        assert!(v["error"].is_null());
        // The summary is flattened into `full`, not nested under it.
        assert_eq!(v["full"]["count"], 35);
        assert_eq!(v["full"]["totalSizeDelta"], 23_000_000);
        assert!(v["quick"]["moved"].is_array());
    }

    #[test]
    fn the_snapshot_round_trips() {
        let (_d, s) = store();
        write_quick(&s, RUNNING);
        let st = s.snapshot("somehost", "/etc/nixos", Some(RUNNING));
        let text = serde_json::to_string(&st).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), st);
    }
}
