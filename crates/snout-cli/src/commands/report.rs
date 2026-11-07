//! Everything the daemon knows, as one document a desktop's updates window
//! renders without knowing anything about snout.
//!
//! `status --json` and `list-updates --json` are data: timestamps, counts, a
//! phase enum, the package list. Turning those into a window means deciding
//! what a phase is called, what the daemon line says while it backs off, and
//! which of five empty states the package list is in — and every surface that
//! did that itself would be one more copy of snout's vocabulary to drift. So
//! this command does it once, here, where the vocabulary lives.
//!
//! The shape is a generic one, not snout's: a headline and a count, a list of
//! labelled notices, and *parts*, each a named list of updates with a note, the
//! command that applies it, and what to say when it is empty. A tool that
//! updates something else entirely (apt, a nix profile, firmware) can produce
//! the same document, and the same window shows either. snout's two parts are
//! the moved flake inputs and the packages they would change.
//!
//! Like `status` and `list-updates`, it reads and never checks: two requests
//! the daemon answers from what it already holds. Unlike them it has no plain
//! form — the terminal already has those two commands — so it requires
//! `--json` rather than inventing a third rendering of the same facts.

use serde::Serialize;

use snout_common::docs::{ErrorDoc, Phase, SCHEMA_VERSION, Status, UpdatesDoc};
use snout_common::fail::{Code, Error, Result};
use snout_common::nix::Kind;
use snout_common::time::Timestamp;

use crate::cli::App;
use crate::ui::{clock, size};

/// The command that applies either part. One command for both, because
/// snout's two parts are one decision: the inputs move and the packages follow.
const APPLY: &str = "snout upgrade";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportDoc {
    pub schema_version: u32,
    /// The window's headline, shown beside `count` when that is non-zero.
    pub title: String,
    /// The number worth putting in large type: packages that would change.
    /// Zero until the package diff exists, however many inputs moved.
    pub count: usize,
    /// The same bit `has-updates` answers: the inputs moved.
    pub available: bool,
    pub busy: bool,
    /// When the inputs were last compared — the "checked at" a reader means.
    pub checked_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    /// What a running check is doing, as a phrase; `null` when idle.
    pub progress: Option<String>,
    /// Checking is failing. Filtered like `has-updates`: a failed package
    /// build leaves the inputs answered, and is reported on the packages part.
    pub error: Option<ErrorDoc>,
    pub notices: Vec<Notice>,
    pub parts: Vec<Part>,
    /// One line on how to act on this, for the window's footer.
    pub hint: String,
    /// Which snout, machine and flake this describes, for the small print.
    pub about: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub label: String,
    pub text: String,
    pub tone: Tone,
}

/// How a notice should read. A surface maps these onto its own colours. The
/// format also has `danger`, which snout never needs: a failure travels in
/// `error`, where a surface already shows it as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Normal,
    Muted,
    Warning,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    pub id: &'static str,
    pub name: &'static str,
    /// A one-line summary beside the part's name; empty for none.
    pub note: String,
    pub apply: &'static str,
    pub checked_at: Option<Timestamp>,
    pub busy: bool,
    pub error: Option<ErrorDoc>,
    /// Shown in place of the list when `updates` is empty.
    pub message: String,
    pub updates: Vec<Item>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub name: String,
    pub from: String,
    pub to: String,
    pub kind: Kind,
    /// Exact signed bytes when the part knows them; absent when it does not
    /// (an input has no size). Whether a small delta is worth showing is the
    /// renderer's call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_delta: Option<i64>,
}

pub async fn run(app: &App) -> Result<()> {
    if !app.json {
        return Err(Error::usage(
            "report only produces JSON — run 'snout report --json', or read the same \
             facts with 'snout status' and 'snout list-updates'",
        ));
    }
    let daemon = app.daemon();
    let st = daemon.status().await?;
    let list = daemon.list_updates().await?;
    app.emit_json(&build(&st, &list));
    Ok(())
}

/// The whole document, from the two answers. Separate from `run` so the
/// wording can be tested without a socket.
pub fn build(st: &Status, list: &UpdatesDoc) -> ReportDoc {
    let d = &st.daemon;
    let (build_error, check_error) = match &st.error {
        Some(e) if e.code == Code::Build => (Some(e.clone()), None),
        other => (None, other.clone()),
    };

    let quick_known = st.quick.checked_at.is_some();
    let moved = st.quick.available;
    let full_known = list.checked_at.is_some();
    let count = if full_known { list.summary.count } else { 0 };

    // Which half of a check is running. A check that never gets past the
    // inputs never builds, so "building" is only claimed once it is true.
    let building = matches!(d.phase, Some(Phase::Building | Phase::Comparing));

    let title = if full_known && count > 0 {
        (if count == 1 {
            "update available"
        } else {
            "updates available"
        })
        .to_string()
    } else if quick_known && moved {
        "Inputs have moved".to_string()
    } else if !quick_known {
        "Not checked yet".to_string()
    } else {
        "Up to date".to_string()
    };

    let progress = d.busy.then(|| {
        d.phase
            .map_or("Starting a check".to_string(), |p| sentence(p.describe()))
    });

    let daemon_line = if d.busy {
        Notice {
            label: "Daemon".into(),
            text: "Checking now".into(),
            tone: Tone::Normal,
        }
    } else if let Some(until) = d.backoff_until {
        Notice {
            label: "Daemon".into(),
            text: format!("Retrying at {} after a failure", clock(until)),
            tone: Tone::Warning,
        }
    } else {
        Notice {
            label: "Daemon".into(),
            text: match d.next_check_at {
                Some(next) => format!("Idle · next check {}", clock(next)),
                None => "Idle".into(),
            },
            tone: Tone::Muted,
        }
    };

    let inputs = Part {
        id: "inputs",
        name: "Flake inputs",
        note: String::new(),
        apply: APPLY,
        checked_at: st.quick.checked_at,
        busy: d.busy && !building,
        error: check_error.clone(),
        message: if quick_known {
            "Up to date".into()
        } else {
            "Never checked".into()
        },
        updates: st
            .quick
            .moved
            .iter()
            .map(|m| Item {
                name: m.name.clone(),
                from: m.from.clone(),
                to: m.to.clone(),
                kind: Kind::Changed,
                size_delta: None,
            })
            .collect(),
    };

    let s = &list.summary;
    let packages = Part {
        id: "packages",
        name: "Packages",
        note: if full_known && count > 0 {
            format!(
                "{} changed, {} added, {} removed · closure {}",
                s.changed,
                s.added,
                s.removed,
                size(s.total_size_delta)
            )
        } else {
            String::new()
        },
        apply: APPLY,
        checked_at: list.checked_at,
        busy: d.busy && building,
        error: build_error.clone(),
        message: if full_known {
            "Nothing would change — you are on the latest of everything".into()
        } else if quick_known && !moved {
            "Nothing to compare — no input has moved".into()
        } else if build_error.is_some() {
            "The inputs have moved, but the updated system failed to build".into()
        } else if d.busy {
            "A check is running; the list appears here if it finds anything to build".into()
        } else if quick_known {
            "Not worked out yet — the daemon builds the updated system on its own schedule".into()
        } else {
            "Never checked — the daemon checks on its own schedule, starting shortly after login"
                .into()
        },
        updates: list
            .updates
            .iter()
            .map(|c| Item {
                name: c.name.clone(),
                from: version(&c.from),
                to: version(&c.to),
                kind: c.kind,
                size_delta: Some(c.size_delta),
            })
            .collect(),
    };

    ReportDoc {
        schema_version: SCHEMA_VERSION,
        title,
        count,
        available: moved,
        busy: d.busy,
        checked_at: st.quick.checked_at,
        started_at: d.started_at,
        progress,
        error: check_error,
        notices: vec![daemon_line],
        parts: vec![inputs, packages],
        hint: format!("check now with `snout check` · apply with `{APPLY}`"),
        about: format!("snout {} · {} · {}", st.version, st.host, st.flake),
    }
}

/// diff-closures' markers for "absent" and "empty version" read as blanks: the
/// kind already says a package was added or removed.
fn version(v: &str) -> String {
    match v {
        "∅" | "ε" => String::new(),
        v => v.to_string(),
    }
}

/// "building the updated system" → "Building the updated system".
fn sentence(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}
