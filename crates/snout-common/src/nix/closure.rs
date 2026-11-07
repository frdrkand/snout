//! Comparing two closures the way `nix store diff-closures` does.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::name::{package_of, show_versions};
use super::pathinfo::{PathInfo, query_closure};
use crate::fail::Result;
use crate::run::Runner;

/// diff-closures' `showDelta` gate: a size change smaller than this is not
/// worth reporting on its own.
pub const SIZE_DELTA_THRESHOLD: i64 = 8 * 1024;

/// What kind of move a [`Change`] describes, as carried in the JSON contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Changed,
    Added,
    Removed,
}

/// One package whose set of versions differs between two closures.
///
/// `from` and `to` are rendered the way diff-closures renders them — `∅` for
/// "not present at all", `ε` for "present with an empty version", and a
/// comma-separated list when one name carries several versions at once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub name: String,
    pub from: String,
    pub to: String,
    /// Exact bytes, signed. Unlike diff-closures, which formats it for a human
    /// and hides anything under 8 KiB, this is always the real number; deciding
    /// whether it is worth showing belongs to the renderer.
    #[serde(rename = "sizeDelta")]
    pub size_delta: i64,
    pub kind: Kind,
}

/// The countable part of a closure diff.
///
/// Flattened into every document that reports one — `updates.json`,
/// `diff --json`, and the status snapshot — so those three cannot drift apart
/// in their field names, their JSON shape, or the arithmetic behind them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub count: usize,
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    /// Counts packages whose closure size moved but whose version did not.
    /// They are not updates and are not listed; the number is reported so the
    /// count can be reconciled with `nix store diff-closures`, which does print
    /// them.
    #[serde(rename = "sizeOnly")]
    pub size_only: usize,
    /// The whole closure's change in bytes, size-only packages included.
    #[serde(rename = "totalSizeDelta")]
    pub total_size_delta: i64,
}

/// The whole comparison.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClosureDiff {
    /// Only packages whose versions moved, ordered changed → added → removed
    /// and sorted by name within each group. Packages that merely changed size
    /// are deliberately absent: they are not updates, and counting them would
    /// inflate the number a status bar shows.
    pub changes: Vec<Change>,
    /// Counts the packages excluded by that rule, so the count can be explained
    /// rather than silently differing from `nix store diff-closures`.
    pub size_only: usize,
    /// The difference in total closure size, over every path, including the
    /// size-only ones.
    pub total_size_delta: i64,
}

impl ClosureDiff {
    /// Reduces the diff to its counts.
    pub fn summary(&self) -> Summary {
        let mut s = Summary {
            count: self.changes.len(),
            size_only: self.size_only,
            total_size_delta: self.total_size_delta,
            ..Default::default()
        };
        for c in &self.changes {
            match c.kind {
                Kind::Changed => s.changed += 1,
                Kind::Added => s.added += 1,
                Kind::Removed => s.removed += 1,
            }
        }
        s
    }
}

#[derive(Default)]
struct Group {
    versions: HashSet<String>,
    size: i64,
}

/// Reduces a closure to nix's `name -> versions, total size` structure.
///
/// Paths that are not shaped like store paths are skipped rather than failing
/// the whole diff — one unparseable entry should not lose the answer — but
/// their size still counts toward the total, which is what makes the closure
/// total agree with what nix reports.
fn group_closure(pi: &PathInfo) -> (HashMap<String, Group>, i64) {
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut total = 0i64;
    for (base, meta) in pi.entries() {
        total += meta.nar_size;
        let Some((name, version)) = package_of(base) else {
            continue;
        };
        let g = groups.entry(name.to_string()).or_default();
        g.versions.insert(version.to_string());
        g.size += meta.nar_size;
    }
    (groups, total)
}

/// Compares two closures.
///
/// A pure function over two parsed path-info documents, which is what makes it
/// testable without a nix store in the loop.
pub fn diff_closures(before: &PathInfo, after: &PathInfo) -> ClosureDiff {
    let (before_groups, before_total) = group_closure(before);
    let (after_groups, after_total) = group_closure(after);

    let names: BTreeSet<&String> = before_groups.keys().chain(after_groups.keys()).collect();

    let (mut changed, mut added, mut removed) = (Vec::new(), Vec::new(), Vec::new());
    let mut size_only = 0usize;

    for name in names {
        let b = before_groups.get(name);
        let a = after_groups.get(name);

        let before_size = b.map_or(0, |g| g.size);
        let after_size = a.map_or(0, |g| g.size);
        let delta = after_size - before_size;

        let gone = missing_versions(b, a);
        let fresh = missing_versions(a, b);

        if gone.is_empty() && fresh.is_empty() {
            // Same versions on both sides. Only a size change is left, and only
            // a large one would even be reported by diff-closures.
            if delta.abs() >= SIZE_DELTA_THRESHOLD {
                size_only += 1;
            }
            continue;
        }

        let (kind, bucket) = if gone.is_empty() {
            (Kind::Added, &mut added)
        } else if fresh.is_empty() {
            (Kind::Removed, &mut removed)
        } else {
            (Kind::Changed, &mut changed)
        };

        bucket.push(Change {
            name: name.clone(),
            from: show_versions(&gone),
            to: show_versions(&fresh),
            size_delta: delta,
            kind,
        });
    }

    // Already name-sorted within each bucket, since `names` is a BTreeSet.
    let mut changes = changed;
    changes.append(&mut added);
    changes.append(&mut removed);

    ClosureDiff {
        changes,
        size_only,
        total_size_delta: after_total - before_total,
    }
}

/// The versions present in `have` but not in `other`, sorted. A missing group
/// has no versions, which is how a package that appears on only one side
/// becomes `∅` on the other.
fn missing_versions(have: Option<&Group>, other: Option<&Group>) -> Vec<String> {
    let Some(have) = have else {
        return Vec::new();
    };
    let mut out: Vec<String> = have
        .versions
        .iter()
        .filter(|v| other.is_none_or(|o| !o.versions.contains(*v)))
        .cloned()
        .collect();
    out.sort();
    out
}

/// The whole operation: query both closures and compare them.
pub async fn diff_paths(r: &Runner, before: &str, after: &str) -> Result<ClosureDiff> {
    let b = query_closure(r, before).await?;
    let a = query_closure(r, after).await?;
    Ok(diff_closures(&b, &a))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a path-info document from `(basename, narSize)` pairs.
    ///
    /// Each basename is prefixed with a distinct 32-character hash, so the
    /// fixture reads as the package names it is about rather than as store
    /// paths — and two entries with the same name still land on separate paths,
    /// which is how a package carries two versions at once.
    fn closure(paths: &[(&str, i64)]) -> PathInfo {
        let mut info = String::from("{\"version\":2,\"storeDir\":\"/nix/store\",\"info\":{");
        for (i, (name, size)) in paths.iter().enumerate() {
            if i > 0 {
                info.push(',');
            }
            // A unique 32-char hash per entry, derived from its index.
            let hash = format!("{:0>32}", format!("{i}"));
            info.push_str(&format!("\"{hash}-{name}\":{{\"narSize\":{size}}}"));
        }
        info.push_str("}}");
        super::super::pathinfo::parse(info.as_bytes()).unwrap()
    }

    fn names(d: &ClosureDiff) -> Vec<(&str, &str, &str, Kind)> {
        d.changes
            .iter()
            .map(|c| (c.name.as_str(), c.from.as_str(), c.to.as_str(), c.kind))
            .collect()
    }

    #[test]
    fn an_unchanged_closure_produces_nothing() {
        let a = closure(&[("hello-2.12", 100), ("glibc-2.42-67", 200)]);
        let d = diff_closures(&a, &a);
        assert!(d.changes.is_empty());
        assert_eq!(d.size_only, 0);
        assert_eq!(d.total_size_delta, 0);
    }

    #[test]
    fn a_version_move_is_a_change() {
        let before = closure(&[("hello-2.12", 100)]);
        let after = closure(&[("hello-2.13", 150)]);
        let d = diff_closures(&before, &after);
        assert_eq!(names(&d), [("hello", "2.12", "2.13", Kind::Changed)]);
        assert_eq!(d.changes[0].size_delta, 50);
        assert_eq!(d.total_size_delta, 50);
    }

    #[test]
    fn one_sided_packages_read_as_the_empty_set() {
        let before = closure(&[("gone-1.0", 100)]);
        let after = closure(&[("fresh-1.0", 300)]);
        let d = diff_closures(&before, &after);
        // Added first, then removed — and the absent side is ∅, not a blank.
        assert_eq!(
            names(&d),
            [
                ("fresh", "∅", "1.0", Kind::Added),
                ("gone", "1.0", "∅", Kind::Removed),
            ]
        );
        assert_eq!(d.total_size_delta, 200);
    }

    #[test]
    fn a_package_with_no_version_reads_as_epsilon() {
        // "firmware" has no version at all. Rendering it as a blank would make
        // the row read "firmware:  → 1.0", which looks like a formatting bug.
        let before = closure(&[("firmware", 100)]);
        let after = closure(&[("firmware-1.0", 100)]);
        let d = diff_closures(&before, &after);
        assert_eq!(names(&d), [("firmware", "ε", "1.0", Kind::Changed)]);
    }

    #[test]
    fn one_name_carrying_two_versions_lists_both() {
        let before = closure(&[("gcc-13.0", 100), ("gcc-14.0", 100)]);
        let after = closure(&[("gcc-15.0", 100)]);
        let d = diff_closures(&before, &after);
        assert_eq!(names(&d), [("gcc", "13.0, 14.0", "15.0", Kind::Changed)]);
    }

    #[test]
    fn a_version_kept_alongside_a_new_one_is_not_reported_as_gone() {
        // Only the versions *missing* from the other side are listed, so a
        // package that gained a second version shows the addition alone.
        let before = closure(&[("openssl-3.0", 100)]);
        let after = closure(&[("openssl-3.0", 100), ("openssl-3.5", 100)]);
        let d = diff_closures(&before, &after);
        assert_eq!(names(&d), [("openssl", "∅", "3.5", Kind::Added)]);
    }

    #[test]
    fn a_size_only_change_is_counted_but_never_listed() {
        // The version set is identical, so this is not an update. Listing it
        // would inflate the number a status bar shows.
        let before = closure(&[("blob-1.0", 1_000_000)]);
        let after = closure(&[("blob-1.0", 2_000_000)]);
        let d = diff_closures(&before, &after);
        assert!(d.changes.is_empty());
        assert_eq!(d.size_only, 1);
        assert_eq!(d.total_size_delta, 1_000_000);
    }

    #[test]
    fn the_size_only_threshold_is_eight_kib_in_both_directions() {
        let cases = [
            (0i64, 0usize),
            (SIZE_DELTA_THRESHOLD - 1, 0),
            (SIZE_DELTA_THRESHOLD, 1),
            (-(SIZE_DELTA_THRESHOLD - 1), 0),
            (-SIZE_DELTA_THRESHOLD, 1),
        ];
        for (delta, want) in cases {
            let before = closure(&[("blob-1.0", 1_000_000)]);
            let after = closure(&[("blob-1.0", 1_000_000 + delta)]);
            let d = diff_closures(&before, &after);
            assert_eq!(d.size_only, want, "delta {delta}");
        }
    }

    #[test]
    fn changes_are_ordered_changed_then_added_then_removed() {
        let before = closure(&[("keep-1.0", 10), ("drop-1.0", 10), ("move-1.0", 10)]);
        let after = closure(&[("keep-1.0", 10), ("gain-1.0", 10), ("move-2.0", 10)]);
        let d = diff_closures(&before, &after);
        assert_eq!(
            names(&d),
            [
                ("move", "1.0", "2.0", Kind::Changed),
                ("gain", "∅", "1.0", Kind::Added),
                ("drop", "1.0", "∅", Kind::Removed),
            ]
        );
    }

    #[test]
    fn each_group_is_sorted_by_name_within_itself() {
        let before = closure(&[("zzz-1.0", 10), ("aaa-1.0", 10)]);
        let after = closure(&[("zzz-2.0", 10), ("aaa-2.0", 10)]);
        let d = diff_closures(&before, &after);
        assert_eq!(
            d.changes
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["aaa", "zzz"]
        );
    }

    #[test]
    fn output_suffixes_group_with_their_package() {
        // libressl-4.3.2-man and libressl-4.3.2 are one package at one version.
        // Grouping them separately would report a spurious change whenever only
        // one of the two outputs is in the closure.
        let before = closure(&[("libressl-4.3.2", 100), ("libressl-4.3.2-man", 50)]);
        let after = closure(&[("libressl-4.3.3", 100), ("libressl-4.3.3-man", 50)]);
        let d = diff_closures(&before, &after);
        assert_eq!(names(&d), [("libressl", "4.3.2", "4.3.3", Kind::Changed)]);
        // Both outputs' bytes belong to the one group.
        assert_eq!(d.changes[0].size_delta, 0);
    }

    #[test]
    fn a_hyphenated_name_is_not_split_at_its_first_dash() {
        let before = closure(&[("bash-interactive-5.3p15", 100)]);
        let after = closure(&[("bash-interactive-5.3p16", 100)]);
        let d = diff_closures(&before, &after);
        assert_eq!(
            names(&d),
            [("bash-interactive", "5.3p15", "5.3p16", Kind::Changed)]
        );
    }

    #[test]
    fn an_entry_that_is_not_a_store_path_is_skipped_but_still_weighed() {
        // Skipping it entirely would make the closure total disagree with what
        // nix reports; failing the whole diff would lose the answer over one
        // stray entry.
        let mut before = closure(&[("hello-1.0", 100)]);
        let after = closure(&[("hello-1.0", 100)]);
        before.info.as_mut().unwrap().insert(
            "not-a-store-path".into(),
            super::super::pathinfo::PathMeta { nar_size: 999 },
        );

        let d = diff_closures(&before, &after);
        assert!(d.changes.is_empty());
        assert_eq!(d.total_size_delta, -999);
    }

    #[test]
    fn the_summary_counts_each_kind() {
        let before = closure(&[("a-1.0", 10), ("b-1.0", 10), ("blob-1.0", 1_000_000)]);
        let after = closure(&[("a-2.0", 10), ("c-1.0", 10), ("blob-1.0", 2_000_000)]);
        let s = diff_closures(&before, &after).summary();
        assert_eq!(
            s,
            Summary {
                count: 3,
                changed: 1,
                added: 1,
                removed: 1,
                size_only: 1,
                total_size_delta: 1_000_000,
            }
        );
    }

    #[test]
    fn the_summary_flattens_into_its_containing_document() {
        // The three documents that report a diff embed this struct rather than
        // spelling the six fields out, so they cannot drift apart.
        let s = Summary {
            count: 2,
            changed: 1,
            added: 1,
            removed: 0,
            size_only: 3,
            total_size_delta: -4096,
        };
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(v["count"], 2);
        assert_eq!(v["sizeOnly"], 3);
        assert_eq!(v["totalSizeDelta"], -4096);
    }

    #[test]
    fn change_kinds_serialise_as_the_contract_spells_them() {
        for (kind, want) in [
            (Kind::Changed, "changed"),
            (Kind::Added, "added"),
            (Kind::Removed, "removed"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{want}\""));
        }
    }

    #[tokio::test]
    async fn diff_paths_queries_before_then_after() {
        let r = Runner::fake();
        r.as_fake().on_func("nix path-info", |c| {
            let path = c.args.last().unwrap();
            let body = if path == "/before" {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-1.0":{"narSize":10}}}"#
            } else {
                r#"{"version":2,"info":{"00000000000000000000000000000000-hello-2.0":{"narSize":10}}}"#
            };
            (
                crate::run::Output {
                    stdout: body.as_bytes().to_vec(),
                    ..Default::default()
                },
                Ok(()),
            )
        });

        let d = diff_paths(&r, "/before", "/after").await.unwrap();
        assert_eq!(names(&d), [("hello", "1.0", "2.0", Kind::Changed)]);
        assert_eq!(r.as_fake().count_calls("nix path-info"), 2);
    }
}
