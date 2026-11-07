//! The snout version, the daemon, and what the cache holds.
//!
//! The one command that still answers with no daemon running. Everything else
//! refuses, because it would have nothing to say; this one has something to say
//! precisely *because* the daemon is missing, and reporting a stopped daemon is
//! the most useful thing snout can do at that moment.

use snout_common::docs::Status;
use snout_common::fail::Result;
use snout_common::version;

use crate::cli::App;
use crate::ui::format::ago;
use crate::ui::{GLYPH_ERROR, GLYPH_OK, clock, now, size, when};

/// The width of the label column — the longest label plus a space, so every
/// value starts in the same place.
const LABEL_WIDTH: usize = 9;

/// What `status --json` emits when there is no daemon.
///
/// A separate shape from [`Status`] on purpose. Synthesising an empty `Status`
/// would report `quick.available: false` and `full.count: 0`, which is a claim
/// about the cache — and with the daemon down, nothing here has read the cache
/// or is entitled to make one.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OfflineDoc<'a> {
    schema_version: u32,
    version: &'a str,
    host: &'a str,
    flake: &'a str,
    daemon: Offline,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Offline {
    running: bool,
}

pub async fn run(app: &App) -> Result<()> {
    let st = match app.daemon().status().await {
        Ok(st) => st,
        Err(e) if e.code == snout_common::fail::Code::Precondition => {
            return offline(app);
        }
        Err(e) => return Err(e),
    };

    if app.json {
        app.emit_json(&st);
        return Ok(());
    }
    print(app, &st);
    Ok(())
}

/// No daemon: report our own version and say plainly that nothing is watching.
///
/// The cache is not read here even though it is a readable file. It belongs to
/// the daemon, and a CLI that reads it directly when the daemon is down is one
/// that has two ways of answering the same question — which is exactly the
/// ambiguity the split exists to remove.
fn offline(app: &App) -> Result<()> {
    if app.json {
        app.emit_json(&OfflineDoc {
            schema_version: snout_common::docs::SCHEMA_VERSION,
            version: &version::short(),
            host: &app.cfg.host,
            flake: &app.cfg.flake,
            daemon: Offline { running: false },
        });
        return Ok(());
    }

    let ui = &app.ui;
    ui.header(&app.cfg.host);
    field(app, "snout", &version::long());
    field(app, "flake", &app.cfg.flake);
    field(app, "daemon", &ui.red("not running"));
    ui.println(&format!(
        "  {:<LABEL_WIDTH$} {}",
        "cache",
        ui.dim("unknown — the daemon owns it")
    ));
    ui.blank();
    ui.status(&format!(
        "start it with 'systemctl --user start {}'",
        snout_common::DAEMON_PROG
    ));
    Ok(())
}

fn print(app: &App, st: &Status) {
    let ui = &app.ui;
    let now = now();

    ui.header(&st.host);
    // The daemon's version, not ours. They are separate binaries, and a new
    // `snout` talking to a daemon systemd has not restarted is worth seeing.
    field(app, "snout", &st.version);
    field(app, "flake", &st.flake);

    let mut daemon = ui.green("running");
    if st.daemon.busy {
        let step = st.daemon.phase.map_or("checking now", |p| p.describe());
        let since = st
            .daemon
            .started_at
            .map(|t| format!(", started {}", ago(Some(t), now)))
            .unwrap_or_default();
        daemon.push_str(&ui.dim(&format!("  {step}{since}")));
    } else if let Some(next) = st.daemon.next_check_at {
        daemon.push_str(&ui.dim(&format!("  next check {}", clock(next))));
    }
    field(app, "daemon", &daemon);

    // A backing-off daemon is not idle, and "next check" alone would not say
    // why the next one is further away than the interval.
    if let Some(until) = st.daemon.backoff_until {
        field(
            app,
            "backoff",
            &ui.yellow(&format!("until {}", clock(until))),
        );
    }
    if let Some(e) = &st.error {
        field(
            app,
            "checks",
            &format!("{} {}", ui.red(GLYPH_ERROR), e.message),
        );
    }

    ui.header("Inputs");
    match st.quick.checked_at {
        None => ui.println(&format!("  {}", ui.dim("never checked"))),
        Some(at) if st.quick.available => ui.println(&format!(
            "  {} moved  {}",
            ui.yellow(&st.quick.moved_count.to_string()),
            ui.dim(&when(Some(at), now))
        )),
        Some(at) => ui.println(&format!(
            "  {} up to date  {}",
            ui.green(GLYPH_OK),
            ui.dim(&when(Some(at), now))
        )),
    }

    ui.header("Packages");
    match st.full.checked_at {
        None => ui.println(&format!("  {}", ui.dim("never checked"))),
        Some(at) => {
            let s = &st.full.summary;
            ui.println(&format!(
                "  {} to update ({} changed, {} added, {} removed)  {}",
                s.count,
                s.changed,
                s.added,
                s.removed,
                ui.dim(&when(Some(at), now))
            ));
            ui.println(&format!("  closure {}", ui.dim(&size(s.total_size_delta))));
        }
    }
    ui.blank();
}

/// One `  label     value` line, with the label column at a fixed width so the
/// values line up however long the labels get.
fn field(app: &App, label: &str, value: &str) {
    app.ui.println(&format!("  {label:<LABEL_WIDTH$} {value}"));
}
