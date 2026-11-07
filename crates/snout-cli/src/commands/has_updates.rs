//! Yes or no.
//!
//! The smallest question snout answers, for a status bar or a shell prompt that
//! wants one bit and does not want to parse a document to get it. Like
//! `list-updates`, it reads and never checks.
//!
//! "Available" means the flake's inputs have moved past what is installed. It
//! deliberately does not mean "a package would change": those are different
//! questions, the second is minutes more expensive to answer, and a machine
//! whose daemon has not got round to the second yet should still be able to say
//! yes.
//!
//! Under `--json` it carries the few facts an indicator needs around that bit —
//! whether it was ever established, whether a check is running, whether
//! checking is failing — so a bar can poll this alone. The plain form stays one
//! bare word, because a script reading it has no use for the rest.

use snout_common::docs::{AvailableDoc, SCHEMA_VERSION};
use snout_common::fail::{Code, Result};

use crate::cli::App;

pub async fn run(app: &App) -> Result<()> {
    let st = app.daemon().status().await?;
    let available = st.quick.available;

    if app.json {
        app.emit_json(&AvailableDoc {
            schema_version: SCHEMA_VERSION,
            available,
            checked_at: st.quick.checked_at,
            busy: st.daemon.busy,
            // A failed package build leaves this answer standing: the inputs
            // were compared, and the build is the package-level question's.
            error: st.error.filter(|e| e.code != Code::Build),
        });
        return Ok(());
    }

    // Bare, with no prefix: this is read by `if [ "$(snout has-updates)" = yes ]`.
    app.ui.println(if available { "yes" } else { "no" });
    Ok(())
}
