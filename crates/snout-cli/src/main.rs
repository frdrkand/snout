//! snout — the half of snout you type.
//!
//! It asks `snout-daemon` what it knows and renders the answer, and it runs the
//! foreground work — build, deploy, upgrade, reload — in this process, because
//! this is the one with a terminal attached and therefore the only one `sudo`
//! can prompt on.
//!
//! It owns no cache. Anything it wants to know about updates, it asks for.

mod cli;
mod client;
mod commands;
mod reload;
mod render;
mod ui;

#[cfg(test)]
mod tests;

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(cli::main(std::env::args_os()))
}
