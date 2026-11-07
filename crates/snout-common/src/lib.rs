//! What `snout` and `snout-daemon` both need.
//!
//! The split between the two binaries is an ownership boundary, and this crate
//! is the seam:
//!
//! ```text
//! snout-daemon   owns the cache. It is the only thing that runs a check, and
//!                the only thing that reads or writes the answers on disk.
//!
//! snout          owns the terminal. It asks the daemon what it knows, renders
//!                it, and runs the foreground work — build, deploy, upgrade —
//!                itself, because that work has a sudo prompt attached.
//!
//! snout-common   the settings both read, the documents that travel between
//!                them, the protocol they travel over, and the subprocess
//!                runner both use to shell out to nix.
//! ```
//!
//! Nothing here decides policy. When to check, what to print, and what to do
//! about a failure all belong to one binary or the other — and because this is
//! a separate crate rather than a `shared` directory, the compiler is what says
//! so.

pub mod config;
pub mod docs;
pub mod fail;
pub mod lockdiff;
pub mod nix;
pub mod proto;
pub mod run;
pub mod time;
pub mod version;

// Not `#[cfg(test)]`: the guard has to serialise environment-touching tests in
// all three crates, and a test-only module in this one is invisible to the
// other two. It is a single function behind a mutex, and dead code in a release
// build.
pub mod testenv;

/// The name the CLI is invoked as, used wherever a message tells the reader
/// what to type next.
pub const PROG: &str = "snout";

/// The name of the daemon binary and its systemd user unit, used wherever a
/// message has to tell the reader which of the two is missing.
pub const DAEMON_PROG: &str = "snout-daemon";
