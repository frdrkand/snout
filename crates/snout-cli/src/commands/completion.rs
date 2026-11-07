//! Shell completion scripts.
//!
//! Generated from the command table rather than hand-written, so they cannot
//! drift from the real subcommand and flag list.

use clap::{Args, CommandFactory};
use clap_complete::Shell;

use crate::cli::{App, Cli};
use snout_common::PROG;
use snout_common::fail::Result;

#[derive(Debug, Args)]
pub struct Options {
    /// Which shell to generate for
    pub shell: Shell,
}

pub fn run(app: &App, o: &Options) -> Result<()> {
    let mut cmd = Cli::command();
    let mut out = Vec::new();
    clap_complete::generate(o.shell, &mut cmd, PROG, &mut out);
    app.ui.out.write(&String::from_utf8_lossy(&out));
    Ok(())
}
