//! What built this binary.
//!
//! Shared rather than CLI-only because the daemon reports its own version in
//! the status snapshot: the two are separate binaries now, and a half-upgraded
//! machine — a new `snout` talking to a daemon systemd has not restarted — is
//! exactly the state that is worth being able to see.

/// Stamped at build time; "dev" and "unknown" when built outside the packaging.
pub const VERSION: &str = match option_env!("SNOUT_VERSION") {
    Some(v) => v,
    None => "dev",
};
pub const COMMIT: &str = match option_env!("SNOUT_COMMIT") {
    Some(v) => v,
    None => "unknown",
};
/// Recorded by the build script from the compiler cargo invoked.
pub const RUSTC: &str = env!("SNOUT_RUSTC_VERSION");

/// The one-line form: `0.1.0 (a1b2c3d)`.
pub fn short() -> String {
    format!("{VERSION} ({COMMIT})")
}

/// The full form, naming the toolchain too.
pub fn long() -> String {
    format!("{VERSION} ({COMMIT}), built with {RUSTC}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_always_reportable() {
        // Built outside the packaging these are placeholders, but they are
        // never empty: `status` prints them unconditionally.
        assert!(!VERSION.is_empty());
        assert!(!COMMIT.is_empty());
        assert!(!RUSTC.is_empty());
        assert!(short().contains(VERSION));
        assert!(long().contains(RUSTC));
    }
}
