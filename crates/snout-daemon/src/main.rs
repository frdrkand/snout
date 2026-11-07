//! snout-daemon — the half of snout that owns the update cache.
//!
//! It answers three questions on a socket and takes one instruction, and in
//! between it checks on a schedule of its own. Nothing a user types starts a
//! check: that is the whole point of it existing, because it means a status bar
//! polling every ten seconds drives no work, and checks still happen on a
//! machine nobody has opened a terminal on.
//!
//! Its argument surface is deliberately three overrides and nothing else.
//! Everything else is configuration, because a daemon started by systemd has
//! nobody to pass flags to it.

mod check;
mod daemon;
mod log;
mod server;
mod status;
mod store;

use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser};
use tokio_util::sync::CancellationToken;

use snout_common::config::{self, Config, LOG_LEVELS};
use snout_common::fail::Result;
use snout_common::run::Runner;
use snout_common::{DAEMON_PROG, version};

// The three settings worth being able to override on a command line. Every
// field is optional so that a flag replaces exactly the setting it names and
// leaves the rest of the loaded configuration alone.
/// The snout update checker
#[derive(Debug, Default, PartialEq, Parser)]
#[command(
    name = DAEMON_PROG,
    after_help = "\
Everything else is read from /etc/snout/config.json (or $SNOUT_CONFIG), then
$XDG_CONFIG_HOME/snout/config.json, then the environment."
)]
struct Args {
    /// Path to the flake to watch
    #[arg(long, value_name = "PATH")]
    flake: Option<String>,

    /// nixosConfigurations attribute to build
    #[arg(long, value_name = "NAME")]
    host: Option<String>,

    /// How much to write to the journal
    ///
    /// The logger floors an unknown level at info, so rejecting one here is
    /// what turns a typo into a message rather than a daemon quieter than
    /// asked for.
    #[arg(long, value_name = "LEVEL", value_parser = LOG_LEVELS)]
    log_level: Option<String>,
}

impl Args {
    /// Overrides exactly the settings the flags name, the same shape the CLI
    /// applies its own `--flake` and `--host` in.
    fn apply(&self, cfg: &mut Config) {
        if let Some(v) = &self.flake {
            cfg.flake = v.clone();
        }
        if let Some(v) = &self.host {
            cfg.host = v.clone();
        }
        if let Some(v) = &self.log_level {
            cfg.log_level = v.clone();
        }
    }

    /// Parses argv, reporting `--version` with the commit and the toolchain
    /// rather than the bare crate version — a new `snout` talking to a daemon
    /// systemd has not restarted is exactly the state worth being able to see.
    fn get() -> Args {
        // Built once at start-up because clap wants a `&'static str`, and the
        // string is only known at runtime.
        static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let m = Args::command()
            .version(VERSION.get_or_init(version::long).as_str())
            .get_matches();
        Args::from_arg_matches(&m).unwrap_or_else(|e| e.exit())
    }
}

fn main() -> ExitCode {
    let args = Args::get();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{DAEMON_PROG}: starting the runtime: {e}");
            return ExitCode::from(snout_common::fail::EXIT_FAILED);
        }
    };

    match rt.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{DAEMON_PROG}: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

async fn run(args: Args) -> Result<()> {
    let mut cfg = config::load()?;
    args.apply(&mut cfg);
    cfg.validate()?;

    let cancel = CancellationToken::new();
    // SIGINT and SIGTERM cancel the work rather than killing the process
    // outright, so a check writing its answer finishes the write and a handler
    // mid-reply finishes the line.
    install_signal_handler(cancel.clone());

    let d = Arc::new(daemon::Daemon::new(cfg, Runner::Exec)?);
    d.run(cancel).await
}

fn install_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        cancel.cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> std::result::Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("snout-daemon").chain(args.iter().copied()))
    }

    #[test]
    fn no_arguments_overrides_nothing() {
        assert_eq!(parse(&[]).unwrap(), Args::default());
    }

    #[test]
    fn a_bad_log_level_is_rejected_rather_than_degrading_silently() {
        // The logger floors an unknown level at info; catching it here is what
        // turns a typo into a message instead of a quieter daemon.
        let err = parse(&["--log-level", "chatty"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(parse(&["--log-level", "debug"]).is_ok());
    }

    #[test]
    fn overrides_replace_only_what_they_name() {
        let mut cfg = Config {
            flake: "/from-file".into(),
            host: "from-file".into(),
            log_level: "warn".into(),
            ..Config::default()
        };
        parse(&["--host", "from-flag"]).unwrap().apply(&mut cfg);
        assert_eq!(cfg.host, "from-flag");
        assert_eq!(cfg.flake, "/from-file");
        assert_eq!(cfg.log_level, "warn");
    }
}
