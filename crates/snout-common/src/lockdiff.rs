//! Which flake inputs moved between two `flake.lock` files?
//!
//! Two consumers share this: the daemon's quick check, which compares the
//! working lock against an updated copy, and `snout upgrade`, which has just
//! rewritten the real one. Keeping one implementation is what makes the table
//! `upgrade` prints read the same as the answer the status bar showed.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::fail::{Code, Error, Result};

/// One lock node whose locked revision differs between the two files.
///
/// `from` and `to` are shortened to seven characters, and a node that exists on
/// only one side reads as `-` on the other — an input gaining or losing an
/// input of its own is a change too, and the alternative is a null that every
/// consumer then has to special-case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Deserialize)]
struct LockFile {
    nodes: Option<HashMap<String, Node>>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(default)]
    locked: Locked,
}

#[derive(Deserialize, Default)]
struct Locked {
    #[serde(default)]
    rev: String,
    #[serde(default, rename = "narHash")]
    nar_hash: String,
}

/// Reduces a lock to `name -> locked revision`.
///
/// A node's identity is its `rev` for the git-ish input types and its
/// `narHash` for the rest, because tarball and path inputs have no rev at all.
///
/// The root node is dropped: it is the flake itself, not one of its inputs.
fn revisions(b: &[u8]) -> Result<HashMap<String, String>> {
    let lf: LockFile =
        serde_json::from_slice(b).map_err(|e| Error::wrap(Code::Internal, e, "flake.lock"))?;
    let Some(nodes) = lf.nodes else {
        return Err(Error::internal("flake.lock: no 'nodes' object"));
    };
    Ok(nodes
        .into_iter()
        .filter(|(name, _)| name != "root")
        .map(|(name, node)| {
            let id = if !node.locked.rev.is_empty() {
                node.locked.rev
            } else {
                node.locked.nar_hash
            };
            (name, id)
        })
        .collect())
}

/// Reports the nodes that moved, sorted by name.
///
/// Every node is compared, not just the ones listed under the root's inputs: an
/// input that carries its own nixpkgs moves the build exactly as much as a
/// top-level one does, so looking only at the root would report "up to date"
/// for a real change.
pub fn diff(before: &[u8], after: &[u8]) -> Result<Vec<Moved>> {
    let b = revisions(before)?;
    let a = revisions(after)?;

    let names: BTreeSet<&String> = b.keys().chain(a.keys()).collect();

    Ok(names
        .into_iter()
        .filter_map(|name| {
            let from = b.get(name).map(String::as_str).unwrap_or("-");
            let to = a.get(name).map(String::as_str).unwrap_or("-");
            (from != to).then(|| Moved {
                name: name.clone(),
                from: short(from),
                to: short(to),
            })
        })
        .collect())
}

/// Reduces an identity to something a human can compare at a glance.
///
/// The `sha256-` prefix comes off first. A narHash is the identity for tarball
/// and path inputs, which have no rev, and its first seven characters are the
/// prefix itself — so every such input would read `sha256- → sha256-`, which is
/// worse than useless: it looks like an input that changed into itself.
fn short(s: &str) -> String {
    let s = s.strip_prefix("sha256-").unwrap_or(s);
    s.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real five-node lock: the root plus four inputs, one of which
    /// (`import-tree`) is a tarball input identified by narHash rather than rev.
    const LOCK: &str = r#"{
      "nodes": {
        "flake-parts": {
          "inputs": { "nixpkgs-lib": ["nixpkgs"] },
          "locked": {
            "lastModified": 1786120000,
            "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "owner": "hercules-ci",
            "repo": "flake-parts",
            "rev": "427bf4bd94ca8b8ab0a48ad3e0e0e0e0e0e0e0e0",
            "type": "github"
          },
          "original": { "owner": "hercules-ci", "repo": "flake-parts", "type": "github" }
        },
        "import-tree": {
          "locked": {
            "lastModified": 1786130000,
            "narHash": "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
            "type": "tarball",
            "url": "https://example.invalid/import-tree.tar.gz"
          },
          "original": { "type": "tarball", "url": "https://example.invalid/import-tree.tar.gz" }
        },
        "nixpkgs": {
          "locked": {
            "lastModified": 1786140000,
            "narHash": "sha256-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=",
            "owner": "nixos",
            "repo": "nixpkgs",
            "rev": "0e251e2f0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d",
            "type": "github"
          },
          "original": { "owner": "nixos", "ref": "nixos-unstable", "repo": "nixpkgs", "type": "github" }
        },
        "root": { "inputs": { "flake-parts": "flake-parts", "nixpkgs": "nixpkgs" } },
        "wrappers": {
          "inputs": { "nixpkgs": ["nixpkgs"] },
          "locked": {
            "lastModified": 1786150000,
            "narHash": "sha256-DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD=",
            "owner": "someone",
            "repo": "wrappers",
            "rev": "6e7f66f1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
            "type": "github"
          },
          "original": { "owner": "someone", "repo": "wrappers", "type": "github" }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    /// Moves one node's rev, the way `nix flake update` would.
    fn with_rev(lock: &str, node: &str, rev: &str) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_str(lock).unwrap();
        v["nodes"][node]["locked"]["rev"] = serde_json::json!(rev);
        serde_json::to_vec(&v).unwrap()
    }

    /// Replaces one node's narHash, for the rev-less inputs.
    fn with_nar_hash(lock: &str, node: &str, hash: &str) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_str(lock).unwrap();
        v["nodes"][node]["locked"]["narHash"] = serde_json::json!(hash);
        serde_json::to_vec(&v).unwrap()
    }

    fn without_node(lock: &str, node: &str) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::from_str(lock).unwrap();
        v["nodes"].as_object_mut().unwrap().remove(node);
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn an_unchanged_lock_reports_nothing() {
        let moved = diff(LOCK.as_bytes(), LOCK.as_bytes()).unwrap();
        assert!(moved.is_empty());
    }

    #[test]
    fn one_moved_input_is_reported_shortened() {
        let after = with_rev(LOCK, "nixpkgs", "e5bdc4a1111111111111111111111111111111ff");
        let moved = diff(LOCK.as_bytes(), &after).unwrap();
        assert_eq!(
            moved,
            vec![Moved {
                name: "nixpkgs".into(),
                from: "0e251e2".into(),
                to: "e5bdc4a".into(),
            }]
        );
    }

    #[test]
    fn results_are_sorted_by_name() {
        let mut after = with_rev(LOCK, "wrappers", "7472616fffffffffffffffffffffffffffffffff");
        after = with_rev(
            std::str::from_utf8(&after).unwrap(),
            "nixpkgs",
            "e5bdc4a1111111111111111111111111111111ff",
        );
        let moved = diff(LOCK.as_bytes(), &after).unwrap();
        assert_eq!(
            moved.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["nixpkgs", "wrappers"]
        );
    }

    #[test]
    fn a_node_present_on_only_one_side_reads_as_a_dash() {
        // An input gaining or losing an input of its own is a real change; a
        // null here would make every consumer special-case it.
        let after = without_node(LOCK, "wrappers");
        let moved = diff(LOCK.as_bytes(), &after).unwrap();
        assert_eq!(
            moved,
            vec![Moved {
                name: "wrappers".into(),
                from: "6e7f66f".into(),
                to: "-".into(),
            }]
        );

        let moved = diff(&after, LOCK.as_bytes()).unwrap();
        assert_eq!(moved[0].from, "-");
        assert_eq!(moved[0].to, "6e7f66f");
    }

    #[test]
    fn the_root_node_is_ignored() {
        // It is the flake itself, not one of its inputs. Its `inputs` map
        // changes whenever an input is renamed, which is not a version move.
        let mut v: serde_json::Value = serde_json::from_str(LOCK).unwrap();
        v["nodes"]["root"]["inputs"]["extra"] = serde_json::json!("nixpkgs");
        let after = serde_json::to_vec(&v).unwrap();
        assert!(diff(LOCK.as_bytes(), &after).unwrap().is_empty());
    }

    #[test]
    fn a_rev_less_input_falls_back_to_its_nar_hash() {
        // Tarball and path inputs have no rev at all; without the fallback they
        // would compare equal forever and never appear as moved.
        let after = with_nar_hash(
            LOCK,
            "import-tree",
            "sha256-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ=",
        );
        let moved = diff(LOCK.as_bytes(), &after).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].name, "import-tree");
    }

    #[test]
    fn nar_hash_inputs_are_shortened_past_the_prefix() {
        // The first seven characters of a narHash are "sha256-", so shortening
        // before stripping would render every tarball input as
        // `sha256- → sha256-` — an input that appears to have changed into
        // itself.
        let after = with_nar_hash(
            LOCK,
            "import-tree",
            "sha256-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ=",
        );
        let moved = diff(LOCK.as_bytes(), &after).unwrap();
        assert_eq!(moved[0].from, "BBBBBBB");
        assert_eq!(moved[0].to, "ZZZZZZZ");
    }

    #[test]
    fn a_rev_wins_over_a_nar_hash_when_both_are_present() {
        // Every github input has both. Comparing narHash instead would report a
        // move whenever nix re-fetched the same revision.
        let after = with_nar_hash(
            LOCK,
            "nixpkgs",
            "sha256-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ=",
        );
        assert!(diff(LOCK.as_bytes(), &after).unwrap().is_empty());
    }

    #[test]
    fn garbage_is_rejected_rather_than_read_as_an_empty_lock() {
        // An empty lock would compare as "every input removed", which reads as
        // a catastrophic change rather than as the parse failure it is.
        let err = diff(b"{not json", LOCK.as_bytes()).unwrap_err();
        assert!(err.to_string().starts_with("flake.lock:"), "{err}");

        let err = diff(br#"{"version":7}"#, LOCK.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("no 'nodes' object"), "{err}");
    }

    #[test]
    fn a_short_identity_is_left_whole() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
        assert_eq!(short("sha256-"), "");
    }
}
