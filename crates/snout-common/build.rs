//! Records the compiler version so `snout status` can report what built the
//! binaries.
//!
//! Cargo hands us `$RUSTC`, so this needs no network, no git and no repository
//! — it works identically inside a build sandbox and outside one.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=SNOUT_VERSION");
    println!("cargo:rerun-if-env-changed=SNOUT_COMMIT");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=SNOUT_RUSTC_VERSION={version}");
}
