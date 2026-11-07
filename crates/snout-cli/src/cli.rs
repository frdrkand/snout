//! The command table, and what every command is handed.
//!
//! Eight commands, in three groups. Three of them — `list-updates`,
//! `has-updates`, `status` — are questions the daemon answers from its cache;
//! they run in microseconds and start nothing. One, `check`, asks the daemon to
//! run its own check now and follows it. The other four do real work in this
//! process, because it is the one with a terminal attached and therefore the
//! only one `sudo` can prompt on.
//!
//! All of them require a daemon. The questions cannot be answered without one,
//! `check` is a request to it, and the work must be able to tell it that the
//! system changed, or the cache is left describing a machine that no longer
//! exists.

use std::ffi::OsString;

use clap::{ArgAction, Args, Parser, Subcommand};

use snout_common::PROG;
use snout_common::config::{self, Config};
use snout_common::docs::FailureDoc;
use snout_common::fail::{Error, Result};
use snout_common::run::Runner;

use crate::client::Client;
use crate::commands;
use crate::ui::{ColorMode, Stream, Ui, should_color};

/// Options every command accepts.
#[derive(Debug, Args, Clone)]
pub struct Global {
    /// Path to the flake to operate on
    ///
    /// Affects only the work this process does. The daemon watches whatever it
    /// was configured with, and `status` reports which.
    #[arg(long, global = true, value_name = "PATH")]
    pub flake: Option<String>,

    /// nixosConfigurations attribute to build
    #[arg(long, global = true, value_name = "NAME")]
    pub host: Option<String>,

    /// Colourise output
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub color: ColorMode,

    /// Emit one JSON document on stdout
    ///
    /// Accepted by list-updates, has-updates and status.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    pub json: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = PROG,
    version,
    about = "Simple NixOS Update Tool",
    after_help = "\
Exit codes: 0 ok, 1 failed, 2 usage, 3 precondition not met.
--json is accepted by: list-updates, has-updates, status.

Every command needs snout-daemon running; it owns the update cache."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub global: Global,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build the system without activating it, then diff it against the running one
    Build(commands::build::Options),
    /// Build and activate the system
    Deploy,
    /// Update the flake inputs, then build and activate
    Upgrade(commands::upgrade::Options),
    /// List the updates the daemon has found
    ListUpdates(commands::list_updates::Options),
    /// Print yes or no, depending on whether updates are available
    HasUpdates,
    /// Ask the daemon to check for updates now, and wait for the answer
    Check(commands::check::Options),
    /// Report the snout version, the daemon, and what the cache holds
    Status,
    /// Restart the desktop components a switch leaves stale
    Reload,
    /// Print a shell completion script
    ///
    /// Hidden because it exists for the packaging to call at build time, not
    /// for anyone to type.
    #[command(hide = true)]
    Completion(commands::completion::Options),
}

impl Command {
    /// Whether this command can produce a JSON document.
    ///
    /// Everything else refuses `--json` rather than ignoring it, so a caller
    /// that expects machine-readable output from a command that cannot produce
    /// it finds out immediately.
    fn speaks_json(&self) -> bool {
        matches!(
            self,
            Command::ListUpdates(_) | Command::HasUpdates | Command::Status
        )
    }

    /// False for the one command that is meaningful on a machine snout has
    /// never been configured on.
    fn needs_config(&self) -> bool {
        !matches!(self, Command::Completion(_))
    }

    fn name(&self) -> &'static str {
        match self {
            Command::Build(_) => "build",
            Command::Deploy => "deploy",
            Command::Upgrade(_) => "upgrade",
            Command::ListUpdates(_) => "list-updates",
            Command::HasUpdates => "has-updates",
            Command::Check(_) => "check",
            Command::Status => "status",
            Command::Reload => "reload",
            Command::Completion(_) => "completion",
        }
    }
}

/// Everything a command needs.
pub struct App {
    pub ui: Ui,
    pub runner: Runner,
    pub cfg: Config,
    pub json: bool,
    /// Where the daemon is expected to be listening. A field rather than a
    /// call, so a test can put a stand-in daemon somewhere else.
    pub socket: std::path::PathBuf,
}

impl App {
    pub fn daemon(&self) -> Client {
        Client::new(&self.socket)
    }

    /// Emits one JSON document on stdout, with a trailing newline.
    pub fn emit_json<T: serde::Serialize>(&self, v: &T) {
        match serde_json::to_string_pretty(v) {
            Ok(text) => self.ui.println(&text),
            Err(e) => self.ui.error(&format!("encoding the answer: {e}")),
        }
    }
}

/// Parses argv, runs the requested command, and returns the process exit code.
pub fn main<I, T>(argv: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{PROG}: ✗ starting the runtime: {e}");
            return snout_common::fail::EXIT_FAILED;
        }
    };
    rt.block_on(run(argv, Stream::Stdout, Stream::Stderr, Runner::Exec))
}

/// One invocation, with its streams and runner injected.
pub async fn run(argv: Vec<OsString>, out: Stream, err: Stream, runner: Runner) -> u8 {
    // Scanned before clap sees argv, because a parse failure has to honour
    // --json too: a consumer that gets an empty stdout cannot tell a rejected
    // flag from a tool that died.
    let wants_json = argv.iter().any(|a| a == "--json");

    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(e) => return report_parse_error(e, wants_json, &out, &err),
    };

    let color = should_color(cli.global.color, &out) && !cli.global.json;
    let ui = Ui::new(out, err, color);

    match dispatch(cli, ui.clone(), runner).await {
        Ok(()) => snout_common::fail::EXIT_OK,
        Err(e) => {
            if wants_json {
                let doc = FailureDoc::of(&e);
                if let Ok(text) = serde_json::to_string_pretty(&doc) {
                    ui.println(&text);
                }
            } else {
                ui.error(&e.to_string());
            }
            e.exit_code()
        }
    }
}

/// Renders clap's own failures in snout's voice.
fn report_parse_error(e: clap::Error, wants_json: bool, out: &Stream, err: &Stream) -> u8 {
    use clap::error::ErrorKind;

    // `--help` and `--version` are successful requests that clap reports as
    // errors; they go to stdout and exit 0.
    if matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        out.write(&e.to_string());
        return snout_common::fail::EXIT_OK;
    }

    let message = e
        .to_string()
        .lines()
        .find_map(|l| l.strip_prefix("error: "))
        .unwrap_or("invalid arguments")
        .to_string();

    if wants_json {
        let doc = FailureDoc::of(&Error::usage(message));
        if let Ok(text) = serde_json::to_string_pretty(&doc) {
            out.write(&format!("{text}\n"));
        }
    } else {
        // The full rendering, so the reader gets clap's usage line too.
        err.write(&e.render().ansi().to_string());
    }
    snout_common::fail::EXIT_USAGE
}

async fn dispatch(cli: Cli, ui: Ui, runner: Runner) -> Result<()> {
    if cli.global.json && !cli.command.speaks_json() {
        return Err(Error::usage(format!(
            "{} does not produce JSON — --json is accepted by: list-updates, has-updates, status",
            cli.command.name()
        )));
    }

    let cfg = if cli.command.needs_config() {
        let mut cfg = config::load()?;
        if let Some(v) = &cli.global.flake {
            cfg.flake = v.clone();
        }
        if let Some(v) = &cli.global.host {
            cfg.host = v.clone();
        }
        cfg.validate()?;
        cfg
    } else {
        Config::default()
    };

    // No signal handling here. Every subprocess this runs is a passthrough
    // child in the same foreground process group, so a Ctrl-C at the terminal
    // reaches `nix build` and `nixos-rebuild` directly — installing a handler
    // would only add a way to swallow it.
    let app = App {
        ui,
        runner,
        cfg,
        json: cli.global.json,
        socket: snout_common::proto::socket_path(),
    };

    match cli.command {
        Command::Build(o) => commands::build::run(&app, &o).await,
        Command::Deploy => commands::deploy::run(&app).await,
        Command::Upgrade(o) => commands::upgrade::run(&app, &o).await,
        Command::ListUpdates(o) => commands::list_updates::run(&app, &o).await,
        Command::HasUpdates => commands::has_updates::run(&app).await,
        Command::Check(o) => commands::check::run(&app, &o).await,
        Command::Status => commands::status::run(&app).await,
        Command::Reload => commands::reload::run(&app).await,
        Command::Completion(o) => commands::completion::run(&app, &o),
    }
}
