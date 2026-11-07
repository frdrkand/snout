//! Checking now, rather than when the schedule next comes round.
//!
//! The daemon still does the work — this command only asks it to bring the
//! scheduled check forward, then follows along through `status`. So a check
//! started here is the same check the schedule would have run: one code path,
//! one lock, one cache, and the status bar sees it like any other.
//!
//! It keeps the cache, unlike the invalidation `deploy` sends: the answers on
//! disk are still true until the new check replaces them. And it retries a
//! lock whose build failed, which the schedule deliberately does not — asking
//! by hand is a request to try again.
//!
//! Interrupting the wait does not stop the check. The daemon owns it, and it
//! carries on in the background exactly as a scheduled one would.

use std::time::Duration;

use clap::{ArgAction, Args};

use snout_common::PROG;
use snout_common::docs::{Phase, Status};
use snout_common::fail::{Error, Result};

use crate::cli::App;
use crate::ui::plural;

/// How often to ask the daemon how the check is getting on. A status call is
/// a file read on the far side, so this is about how quickly a phase change
/// shows up, not about cost.
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Args)]
pub struct Options {
    /// Return once the daemon has the request, without waiting for the result
    #[arg(long, action = ArgAction::SetTrue)]
    pub no_wait: bool,
}

pub async fn run(app: &App, o: &Options) -> Result<()> {
    let daemon = app.daemon();
    let started = daemon.check().await?;

    if o.no_wait {
        app.ui.status(if started {
            "check started"
        } else {
            "a check is already running"
        });
        app.ui.status(&format!("follow it with '{PROG} status'"));
        return Ok(());
    }

    if !started {
        app.ui.status("a check is already running — following it");
    }

    let mut last: Option<Phase> = None;
    loop {
        let st = daemon.status().await?;
        if !st.daemon.busy {
            return report(app, &st);
        }
        if st.daemon.phase.is_some() && st.daemon.phase != last {
            last = st.daemon.phase;
            if let Some(p) = last {
                app.ui.status(p.describe());
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Says what the check found, in the same terms `status` would.
///
/// A failure is returned rather than printed, so the exit status carries the
/// code the daemon recorded: 1 for a failed fetch or build, as if the work had
/// happened here.
fn report(app: &App, st: &Status) -> Result<()> {
    if let Some(e) = &st.error {
        return Err(Error::new(e.code, e.message.clone()));
    }

    match st.quick.checked_at {
        // A check that succeeded but published nothing: the running system
        // changed under it, and an answer about the previous one was dropped.
        None => {
            app.ui
                .warn("no answer — the running system changed during the check");
            return Ok(());
        }
        Some(_) if !st.quick.available => app.ui.ok("inputs up to date"),
        Some(_) => app.ui.status(&format!(
            "{} moved",
            plural(st.quick.moved_count, "input", "inputs")
        )),
    }

    if st.quick.available && st.full.checked_at.is_some() {
        let s = &st.full.summary;
        app.ui.status(&format!(
            "{} ({} changed, {} added, {} removed) — see '{PROG} list-updates'",
            plural(s.count, "update", "updates"),
            s.changed,
            s.added,
            s.removed
        ));
    }
    Ok(())
}
