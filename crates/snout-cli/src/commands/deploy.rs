//! Activating a system, and dealing with the cache and the desktop afterwards.

use snout_common::PROG;
use snout_common::fail::{Code, Context, Result};
use snout_common::run::Cmd;

use crate::cli::App;
use crate::commands::reload;
use crate::reload::Reloader;

pub async fn run(app: &App) -> Result<()> {
    app.daemon().require().await?;
    activate(app).await?;
    invalidate_cache(app).await;
    after_switch(app).await
}

/// The one privileged command line, in one place.
///
/// sudo is invoked by bare name so it resolves to the setuid wrapper on PATH.
/// snout never re-executes itself under sudo: the privileged part is exactly
/// one command, and keeping it that way is what makes it obvious what runs as
/// root.
///
/// This is also the reason the switch happens here rather than in the daemon.
/// sudo needs a terminal to prompt on, and the daemon has none — moving this
/// across the socket would mean either passwordless sudo or a root daemon, and
/// neither is a price worth paying to make a diagram tidier.
pub fn switch_argv(app: &App) -> Vec<String> {
    vec![
        "sudo".to_string(),
        "nixos-rebuild".to_string(),
        "switch".to_string(),
        "--flake".to_string(),
        app.cfg.target(),
    ]
}

/// The switch itself, without the follow-up.
pub async fn activate(app: &App) -> Result<()> {
    let (_, res) = app
        .runner
        .run(&Cmd::new(switch_argv(app)).passthrough())
        .await;
    res.context(Code::Build, "switch failed")?;
    app.ui.ok("system activated");
    Ok(())
}

/// Tells the daemon that what it has cached is about a system that has this
/// second stopped running.
///
/// Failures are reported and then dropped. The switch has happened; a daemon
/// that has gone away since the check at the top of the command is no reason to
/// fail a deploy that succeeded, and the daemon's own schedule will correct the
/// cache within the hour regardless.
pub async fn invalidate_cache(app: &App) {
    if let Err(e) = app.daemon().invalidate().await {
        app.ui
            .warn(&format!("could not refresh the update cache: {e}"));
    }
}

/// Either reloads the desktop or says that it might want reloading.
///
/// There is no staleness detection behind this: it is a reminder, not a
/// diagnosis. Turning it into an automatic reload is a configuration choice.
pub async fn after_switch(app: &App) -> Result<()> {
    let r = Reloader::new(app.cfg.reload.clone(), app.runner.clone());

    // `has_work` rather than a components check: a configuration with only post
    // commands is one `reload` runs happily, and requiring a component here
    // would make `afterDeploy = true` silently do nothing for it.
    if app.cfg.reload.after_deploy && r.has_work() {
        return reload::report(app, &r).await;
    }
    if r.hint_applies() {
        app.ui.status(&format!(
            "desktop components may be stale — run '{PROG} reload'"
        ));
    }
    Ok(())
}
