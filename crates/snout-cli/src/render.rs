//! Rendering a package list.
//!
//! Two commands describe the same thing in different words — `build` compared
//! what it just built against what is running, `list-updates` reports what the
//! daemon found — but everything below the first line is identical, so it is
//! written once.

use snout_common::docs::CURRENT_SYSTEM_LINK;
use snout_common::fail::{Code, Error, Result};
use snout_common::nix::{self, Change, SIZE_DELTA_THRESHOLD};

use crate::cli::App;
use crate::ui::{Row, Style, plural, size};

/// The path a build is compared against.
///
/// Handed to nix unresolved — it resolves the link itself, and the unresolved
/// form is what reads well in a message.
pub const RUNNING_SYSTEM: &str = CURRENT_SYSTEM_LINK;

/// Compares two closures and prints the difference.
pub async fn show_diff(app: &App, before: &str, after: &str) -> Result<()> {
    let diff = nix::diff_paths(&app.runner, before, after)
        .await
        .map_err(|e| Error::wrap(Code::Internal, e, "comparing closures"))?;
    let summary = diff.summary();

    if summary.count == 0 {
        app.ui.ok(&format!(
            "no package changes (closure {})",
            size(summary.total_size_delta)
        ));
        return Ok(());
    }

    print_changes(
        app,
        &format!(
            "{} ({} changed, {} added, {} removed)",
            plural(summary.count, "package differs", "packages differ"),
            summary.changed,
            summary.added,
            summary.removed
        ),
        &diff.changes,
        summary.total_size_delta,
    );
    Ok(())
}

/// Renders a package list under a caller-supplied summary line.
pub fn print_changes(app: &App, head: &str, changes: &[Change], total_size_delta: i64) {
    app.ui.status(head);
    app.ui.header("Packages");
    app.ui.table(&change_rows(changes));
    app.ui.blank();
    app.ui
        .status(&format!("closure {}", size(total_size_delta)));
}

pub fn change_rows(changes: &[Change]) -> Vec<Row> {
    changes
        .iter()
        .map(|c| {
            let mut row = Row::new(&c.name, &c.from, &c.to).style(match c.kind {
                nix::Kind::Removed => Style::Removed,
                _ => Style::Arriving,
            });
            // Matching nix's own threshold, so a size shown here is a size
            // diff-closures would have shown too.
            if c.size_delta.abs() >= SIZE_DELTA_THRESHOLD {
                row = row.note(size(c.size_delta));
            }
            row
        })
        .collect()
}

/// The `name  from → to` table of inputs that moved.
///
/// Shared between `list-updates --quick` and `upgrade`, which rewrites the real
/// lock and then has exactly the same thing to say about it — a list of full
/// `github:owner/repo/<40 hex>` refs is what nix prints, and it is not what a
/// reader wants.
pub fn print_moved(app: &App, moved: &[snout_common::lockdiff::Moved]) {
    if moved.is_empty() {
        return;
    }
    app.ui.header("Flake inputs");
    let rows: Vec<Row> = moved
        .iter()
        .map(|m| Row::new(&m.name, &m.from, &m.to))
        .collect();
    app.ui.table(&rows);
    app.ui.blank();
}
