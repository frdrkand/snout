//! What an update would bring in.
//!
//! Purely a read. This command never checks anything and never builds anything
//! — it prints what the daemon has, and if the daemon has nothing it says so.
//! That is what makes it safe for a status bar to call on a window open: the
//! expensive answer was computed hours ago, at the moment the cheap one first
//! knew it was needed.

use clap::{ArgAction, Args};

use snout_common::docs::{MovedDoc, UpdatesDoc};
use snout_common::fail::Result;

use crate::cli::App;
use crate::render;
use crate::ui::{plural, when};

#[derive(Debug, Args)]
pub struct Options {
    /// Report which inputs moved, rather than which packages would change
    ///
    /// The cheap half of what the daemon knows: it says upstream has moved,
    /// not what moving with it would install.
    #[arg(long, action = ArgAction::SetTrue)]
    pub quick: bool,
}

pub async fn run(app: &App, o: &Options) -> Result<()> {
    let daemon = app.daemon();
    if o.quick {
        let doc = daemon.list_moved().await?;
        return print_moved(app, &doc);
    }
    let doc = daemon.list_updates().await?;
    print_updates(app, &doc)
}

fn print_updates(app: &App, d: &UpdatesDoc) -> Result<()> {
    if app.json {
        app.emit_json(d);
        return Ok(());
    }

    let now = crate::ui::now();
    let Some(at) = d.checked_at else {
        // Deliberately not "up to date": nothing has been checked, and saying
        // otherwise would be an answer the daemon never gave.
        app.ui.status("nothing checked yet");
        return Ok(());
    };

    if d.summary.count == 0 {
        app.ui
            .ok(&format!("up to date — checked {}", when(Some(at), now)));
        return Ok(());
    }

    render::print_changes(
        app,
        &format!(
            "{} ({} changed, {} added, {} removed) — checked {}",
            plural(d.summary.count, "update", "updates"),
            d.summary.changed,
            d.summary.added,
            d.summary.removed,
            when(Some(at), now)
        ),
        &d.updates,
        d.summary.total_size_delta,
    );
    Ok(())
}

fn print_moved(app: &App, d: &MovedDoc) -> Result<()> {
    if app.json {
        app.emit_json(d);
        return Ok(());
    }

    let now = crate::ui::now();
    let Some(at) = d.checked_at else {
        app.ui.status("nothing checked yet");
        return Ok(());
    };

    if d.moved_count == 0 {
        app.ui.ok(&format!(
            "inputs up to date — checked {}",
            when(Some(at), now)
        ));
        return Ok(());
    }

    app.ui.status(&format!(
        "{} moved — checked {}",
        plural(d.moved_count, "input", "inputs"),
        when(Some(at), now)
    ));
    render::print_moved(app, &d.moved);
    Ok(())
}
