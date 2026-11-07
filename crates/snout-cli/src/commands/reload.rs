//! Restarting the desktop components a switch leaves stale.

use crate::cli::App;
use crate::reload::Reloader;
use snout_common::fail::Result;

pub async fn run(app: &App) -> Result<()> {
    // Nothing here needs the daemon. It is required anyway, for the same
    // reason `build` requires it: on a machine where nothing is watching for
    // updates, saying so is more useful than quietly working.
    app.daemon().require().await?;

    let r = Reloader::new(app.cfg.reload.clone(), app.runner.clone());
    report(app, &r).await
}

/// Runs a reload and prints what it did.
///
/// The report is printed whether or not the reload succeeded: a partial reload
/// still restarted things, and the caller needs to know which.
pub async fn report(app: &App, r: &Reloader) -> Result<()> {
    let (rep, err) = r.run().await;

    if !rep.restarted.is_empty() {
        app.ui
            .ok(&format!("restarted {}", rep.restarted.join(", ")));
    }
    for f in &rep.failed {
        app.ui.warn(&format!("{}: {}", f.name, f.error));
    }
    for n in &rep.notes {
        app.ui.status(n);
    }

    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
