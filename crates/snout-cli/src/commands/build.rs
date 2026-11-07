//! Building the system without activating it.

use clap::Args;

use snout_common::fail::{Code, Context, Result};
use snout_common::run::Cmd;

use crate::cli::App;
use crate::render;

#[derive(Debug, Args)]
pub struct Options {
    /// Where to place the result symlink
    #[arg(long, value_name = "PATH")]
    pub out_link: Option<String>,
}

pub async fn run(app: &App, o: &Options) -> Result<()> {
    // Nothing here needs the daemon, but the rule that every command requires
    // one is worth more than the exception would save: a machine with no daemon
    // is one where nothing is watching for updates, and finding that out when
    // you type `build` is better than finding it out a fortnight later.
    app.daemon().require().await?;

    let link = match &o.out_link {
        Some(p) => std::path::PathBuf::from(p),
        None => app.cfg.build_link()?,
    };
    build_system(app, &link).await?;
    render::show_diff(app, render::RUNNING_SYSTEM, &link.to_string_lossy()).await
}

/// Builds the configuration without activating it, leaving the result at
/// `link`.
///
/// Unprivileged, and the out-link is a GC root, so what it downloads survives
/// until it is either deployed or collected on purpose.
///
/// `nix build` on the toplevel rather than `nixos-rebuild build`: the ng
/// rewrite of nixos-rebuild dropped `--out-link` entirely, and placing the
/// symlink is the whole point of this command. It is also exactly what the
/// daemon's own check builds, so the two agree by construction.
pub async fn build_system(app: &App, link: &std::path::Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(Code::Internal, || format!("creating {}", parent.display()))?;
    }

    let cmd = Cmd::new([
        "nix",
        "build",
        &app.cfg.toplevel_attr(&app.cfg.flake),
        "--out-link",
        &link.to_string_lossy(),
    ])
    .passthrough();

    let (_, res) = app.runner.run(&cmd).await;
    res.with_context(Code::Build, || format!("building {}", app.cfg.target()))?;

    app.ui.ok(&format!("built {}", link.display()));
    Ok(())
}
