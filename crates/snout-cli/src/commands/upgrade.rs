//! Updating flake inputs, then deploying.

use clap::{ArgAction, Args};

use snout_common::fail::{Code, Context, Result};
use snout_common::lockdiff;
use snout_common::run::Cmd;

use crate::cli::App;
use crate::commands::deploy;
use crate::render;
use crate::ui::plural;

#[derive(Debug, Args)]
pub struct Options {
    /// Update the lock and stop, without building or switching
    #[arg(long, action = ArgAction::SetTrue)]
    pub no_deploy: bool,
}

pub async fn run(app: &App, o: &Options) -> Result<()> {
    app.daemon().require().await?;

    // Read before nix rewrites it, so the update can be summarised as a table
    // instead of being left as nix's own bullet list of forty-character
    // revisions.
    let lock_path = std::path::Path::new(&app.cfg.flake).join("flake.lock");
    let before = std::fs::read(&lock_path).ok();

    app.ui.status("updating flake inputs");
    let cmd = Cmd::new(["nix", "flake", "update", "--flake", &app.cfg.flake]).passthrough();
    let (_, res) = app.runner.run(&cmd).await;
    res.context(Code::Network, "updating flake inputs")?;

    report_moved(app, before.as_deref(), &lock_path);

    if !o.no_deploy {
        app.ui.status("building and switching");
        deploy::activate(app).await?;
    }

    // Both paths reach here. Even with --no-deploy nothing was activated, the
    // working tree's lock has moved — so the cached answer, which is about the
    // lock as it was, has stopped describing this flake.
    deploy::invalidate_cache(app).await;

    // Only when something was activated. With --no-deploy nothing was, so a
    // reload hint would be advice about a system that is not running.
    if !o.no_deploy {
        deploy::after_switch(app).await?;
    }
    Ok(())
}

/// Says which inputs the update actually moved.
///
/// Nothing here can fail the update: the lock has already been rewritten by the
/// time this runs, and refusing to continue because the summary could not be
/// produced would leave a moved lock and an unbuilt system. An unreadable or
/// unparsable lock therefore costs the table, and nothing else.
fn report_moved(app: &App, before: Option<&[u8]>, lock_path: &std::path::Path) {
    let (Some(before), Ok(after)) = (before, std::fs::read(lock_path)) else {
        return;
    };
    let Ok(moved) = lockdiff::diff(before, &after) else {
        return;
    };
    if moved.is_empty() {
        app.ui.ok("inputs already up to date");
        return;
    }
    render::print_moved(app, &moved);
    app.ui
        .status(&format!("{} moved", plural(moved.len(), "input", "inputs")));
}
