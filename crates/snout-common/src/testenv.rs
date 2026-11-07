//! One lock for every test that touches the process environment.
//!
//! `set_var` is process-wide, so a config test rewriting `$HOME` while a
//! command test reads it produces failures that look like logic bugs and move
//! around between runs. Every such test takes this same guard — one per crate,
//! not one per module, which is the whole point.

/// Serialises tests that read or write the process environment.
pub fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
