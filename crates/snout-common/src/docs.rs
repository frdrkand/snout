//! The documents snout writes, serves and prints.
//!
//! Two of them are the daemon's cache files, written after a check and read
//! back on every request:
//!
//! ```text
//! quick.json    Have the flake's inputs moved past what is installed?
//!               Cheap: a lock comparison. Network only, seconds.
//!
//! updates.json  Which packages would actually change? Expensive: a build of
//!               the updated system and a closure diff against the running one.
//!               So a count of 87 means 87 packages change, not 4 inputs moved.
//! ```
//!
//! The other three — [`Status`], [`UpdatesDoc`], [`MovedDoc`] — are what the
//! daemon answers with. The last two exist so an answer has the same shape
//! whether or not the cache holds anything: a consumer never has to distinguish
//! "no document" from "an empty document", because there is no such thing as no
//! document.

use serde::{Deserialize, Serialize};

use crate::fail;
use crate::lockdiff::Moved;
use crate::nix::{Change, Summary};
use crate::time::Timestamp;

/// Carried by every document snout writes or prints. It exists so a consumer
/// can refuse to guess at a shape it does not know, rather than silently
/// reading a field that has changed meaning.
pub const SCHEMA_VERSION: u32 = 1;

/// The profile symlink pointing at the running system.
pub const CURRENT_SYSTEM_LINK: &str = "/run/current-system";

// ---------------------------------------------------------------------------
// The cache files
// ---------------------------------------------------------------------------

/// `quick.json` — the cheap answer, about the flake's inputs.
///
/// It cannot see undeployed local changes, because those do not move the lock:
/// it answers "upstream has moved", not "you have something to deploy". The
/// package-level answer, comparing against the running system, sees both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quick {
    pub schema_version: u32,
    pub checked_at: Timestamp,
    pub available: bool,
    pub moved_count: usize,
    pub moved: Vec<Moved>,
    /// Identifies the lock this answer was computed from. With
    /// `current_system`, the pair decides whether the package diff on disk
    /// still describes reality.
    pub lock_hash: String,
    pub current_system: String,
    pub host: String,
    pub flake: String,
}

/// `updates.json` — the expensive answer, about packages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Full {
    pub schema_version: u32,
    pub checked_at: Timestamp,
    pub lock_hash: String,
    pub current_system: String,
    pub host: String,
    pub flake: String,
    /// Flattened, so this document and the status snapshot cannot drift apart
    /// in their field names or their arithmetic.
    #[serde(flatten)]
    pub summary: Summary,
    pub updates: Vec<Change>,
}

// ---------------------------------------------------------------------------
// The answers
// ---------------------------------------------------------------------------

/// What `list-updates` prints: the package diff, or the same shape saying
/// nothing has been checked.
///
/// `checkedAt` is the only field that distinguishes the two, and it is `null`
/// rather than absent — a key that comes and goes makes a consumer branch on
/// its existence as well as its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatesDoc {
    pub schema_version: u32,
    pub checked_at: Option<Timestamp>,
    #[serde(flatten)]
    pub summary: Summary,
    pub updates: Vec<Change>,
}

impl UpdatesDoc {
    pub fn empty() -> UpdatesDoc {
        UpdatesDoc {
            schema_version: SCHEMA_VERSION,
            checked_at: None,
            summary: Summary::default(),
            updates: Vec::new(),
        }
    }
}

impl From<Option<Full>> for UpdatesDoc {
    fn from(f: Option<Full>) -> UpdatesDoc {
        match f {
            None => UpdatesDoc::empty(),
            Some(f) => UpdatesDoc {
                schema_version: SCHEMA_VERSION,
                checked_at: Some(f.checked_at),
                summary: f.summary,
                updates: f.updates,
            },
        }
    }
}

/// What `list-updates --quick` prints: which inputs moved, or the same shape
/// saying nothing has been checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MovedDoc {
    pub schema_version: u32,
    pub checked_at: Option<Timestamp>,
    pub available: bool,
    pub moved_count: usize,
    pub moved: Vec<Moved>,
}

impl MovedDoc {
    pub fn empty() -> MovedDoc {
        MovedDoc {
            schema_version: SCHEMA_VERSION,
            checked_at: None,
            available: false,
            moved_count: 0,
            moved: Vec::new(),
        }
    }
}

impl From<Option<Quick>> for MovedDoc {
    fn from(q: Option<Quick>) -> MovedDoc {
        match q {
            None => MovedDoc::empty(),
            Some(q) => MovedDoc {
                schema_version: SCHEMA_VERSION,
                checked_at: Some(q.checked_at),
                available: q.available,
                moved_count: q.moved_count,
                moved: q.moved,
            },
        }
    }
}

/// What `has-updates` prints under `--json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableDoc {
    pub schema_version: u32,
    pub available: bool,
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

/// What is known right now, without checking anything.
///
/// It deliberately omits the package list. That list can run to a hundred
/// entries, and a surface polling every ten seconds would be re-parsing ten
/// kilobytes of JSON to learn a number that changes twice a day. A consumer
/// that wants the list asks for it separately, and re-asks only when
/// `full.checkedAt` changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub schema_version: u32,
    /// The *daemon's* version, which is not necessarily the CLI's — they are
    /// separate binaries now, and a half-upgraded machine should say so.
    pub version: String,
    pub host: String,
    pub flake: String,
    pub daemon: DaemonStatus,
    pub quick: QuickStatus,
    pub full: FullStatus,
    /// Reports that checking itself is failing, which neither cached answer can
    /// say: a failed check deliberately never overwrites a good one, so both
    /// documents below describe the last time things worked. Without this a
    /// surface could sit on a week-old answer with no way to tell.
    pub error: Option<ErrorDoc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub running: bool,
    /// True while a check is in flight, which is why a status taken now might
    /// differ from one taken in a minute.
    pub busy: bool,
    pub next_check_at: Option<Timestamp>,
    pub backoff_until: Option<Timestamp>,
}

/// Carries the moved-input list, which is small and is what an indicator shows
/// in a tooltip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickStatus {
    pub checked_at: Option<Timestamp>,
    pub available: bool,
    pub moved_count: usize,
    pub moved: Vec<Moved>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullStatus {
    pub checked_at: Option<Timestamp>,
    #[serde(flatten)]
    pub summary: Summary,
}

// ---------------------------------------------------------------------------
// Errors on the wire
// ---------------------------------------------------------------------------

/// The contract's error object: a code and a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorDoc {
    pub code: fail::Code,
    pub message: String,
}

impl From<&fail::Error> for ErrorDoc {
    fn from(e: &fail::Error) -> ErrorDoc {
        ErrorDoc {
            code: e.code,
            message: e.to_string(),
        }
    }
}

impl From<ErrorDoc> for fail::Error {
    /// Rebuilds the error on the far side of the socket, so the exit code is
    /// identical whether the daemon or the CLI hit the failure.
    fn from(d: ErrorDoc) -> fail::Error {
        fail::Error::new(d.code, d.message)
    }
}

/// The minimal document a failing `--json` command emits.
///
/// The invariant for every command that accepts `--json`: stdout carries
/// exactly one JSON document and nothing else, including when the command
/// fails. A consumer must never have to distinguish "empty stdout" from "the
/// tool died".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureDoc {
    pub schema_version: u32,
    pub error: Option<ErrorDoc>,
}

impl FailureDoc {
    pub fn of(e: &fail::Error) -> FailureDoc {
        FailureDoc {
            schema_version: SCHEMA_VERSION,
            error: Some(ErrorDoc::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::now;

    fn quick() -> Quick {
        Quick {
            schema_version: SCHEMA_VERSION,
            checked_at: now(),
            available: true,
            moved_count: 2,
            moved: vec![Moved {
                name: "nixpkgs".into(),
                from: "aaa".into(),
                to: "bbb".into(),
            }],
            lock_hash: "hash".into(),
            current_system: "/nix/store/aaa-system".into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
        }
    }

    #[test]
    fn an_empty_answer_has_the_same_shape_as_a_full_one() {
        // The whole reason these types exist: "the cache is cold" must not be a
        // different document from "nothing has changed", or every consumer
        // grows a branch for it.
        let cold = serde_json::to_value(UpdatesDoc::empty()).unwrap();
        let warm = serde_json::to_value(UpdatesDoc::from(Some(Full {
            schema_version: SCHEMA_VERSION,
            checked_at: now(),
            lock_hash: "h".into(),
            current_system: "/nix/store/aaa".into(),
            host: "somehost".into(),
            flake: "/etc/nixos".into(),
            summary: Summary::default(),
            updates: vec![],
        })))
        .unwrap();

        let mut cold_keys: Vec<_> = cold.as_object().unwrap().keys().collect();
        let mut warm_keys: Vec<_> = warm.as_object().unwrap().keys().collect();
        cold_keys.sort();
        warm_keys.sort();
        assert_eq!(cold_keys, warm_keys);

        // And the only field that says which is which is checkedAt.
        assert!(cold["checkedAt"].is_null());
        assert!(warm["checkedAt"].is_string());
    }

    #[test]
    fn an_empty_list_is_an_array_never_null() {
        // A consumer should iterate it without a null check.
        let v = serde_json::to_value(UpdatesDoc::empty()).unwrap();
        assert_eq!(v["updates"], serde_json::json!([]));
        let v = serde_json::to_value(MovedDoc::empty()).unwrap();
        assert_eq!(v["moved"], serde_json::json!([]));
    }

    #[test]
    fn the_answers_use_the_contract_field_names() {
        let v = serde_json::to_value(MovedDoc::from(Some(quick()))).unwrap();
        assert_eq!(v["schemaVersion"], 1);
        assert_eq!(v["available"], true);
        assert_eq!(v["movedCount"], 2);
        assert_eq!(v["moved"][0]["name"], "nixpkgs");
        // The cache's private bookkeeping does not travel: a consumer has no
        // use for the lock hash, and it would only invite branching on it.
        assert!(v.get("lockHash").is_none(), "{v}");
        assert!(v.get("currentSystem").is_none(), "{v}");
    }

    #[test]
    fn a_full_answer_flattens_its_summary() {
        let f = Full {
            schema_version: SCHEMA_VERSION,
            checked_at: now(),
            lock_hash: "h".into(),
            current_system: "/nix/store/aaa".into(),
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
        let v = serde_json::to_value(UpdatesDoc::from(Some(f))).unwrap();
        assert_eq!(v["count"], 35);
        assert_eq!(v["totalSizeDelta"], 23_000_000);
        assert!(v.get("summary").is_none(), "summary was nested, not flat");
    }

    #[test]
    fn the_documents_round_trip() {
        for v in [
            serde_json::to_string(&UpdatesDoc::empty()).unwrap(),
            serde_json::to_string(&MovedDoc::from(Some(quick()))).unwrap(),
        ] {
            assert!(serde_json::from_str::<serde_json::Value>(&v).is_ok());
        }
        let d = MovedDoc::from(Some(quick()));
        let text = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<MovedDoc>(&text).unwrap(), d);
    }

    #[test]
    fn an_error_survives_the_round_trip_with_its_code() {
        // The exit code must be identical whether the daemon or the CLI hit the
        // failure, which means the code has to travel, not just the text.
        let original = fail::Error::network("updating flake inputs: timed out");
        let doc = ErrorDoc::from(&original);
        let rebuilt: fail::Error = doc.clone().into();
        assert_eq!(rebuilt.code, fail::Code::Network);
        assert_eq!(rebuilt.exit_code(), original.exit_code());
        assert_eq!(rebuilt.to_string(), original.to_string());

        assert_eq!(serde_json::to_value(&doc).unwrap()["code"], "network");
    }
}
