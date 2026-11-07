//! snout's human-facing output.
//!
//! Only the terminal's own ANSI 16 palette is used, never a hardcoded hex
//! colour. That is what keeps the tool standalone: it inherits whatever theme
//! the terminal is already wearing, and it degrades honestly on a terminal that
//! has no colour at all.

pub mod format;
pub mod table;

use std::io::{IsTerminal, Write};
#[cfg(test)]
use std::sync::{Arc, Mutex};

pub use format::{plural, size, when};
pub use table::{Row, Style};

// Re-exported so a caller writing a line does not have to reach into two crates
// to name a time and then render it.
pub use snout_common::PROG;
pub use snout_common::time::{clock, now};

/// Colour modes accepted by `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

// ANSI 16 only. Bright variants are avoided, because a terminal with a light
// background renders some of them almost invisibly.
pub(crate) const RESET: &str = "\x1b[0m";
pub(crate) const BOLD: &str = "\x1b[1m";
pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const RED: &str = "\x1b[31m";
pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const YELLOW: &str = "\x1b[33m";

/// Glyphs for the three outcomes worth marking. Emitted regardless of colour:
/// stripping them along with the escapes would lose the distinction entirely on
/// a terminal that has no colour.
pub const GLYPH_OK: &str = "✓";
pub const GLYPH_WARN: &str = "!";
pub const GLYPH_ERROR: &str = "✗";

/// Where output goes.
///
/// The buffer variant is what lets a test read back exactly what a command
/// printed, including the escape sequences, without a terminal in the loop.
#[derive(Clone)]
pub enum Stream {
    Stdout,
    Stderr,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

impl Stream {
    #[cfg(test)]
    pub fn buffer() -> Stream {
        Stream::Buffer(Arc::new(Mutex::new(Vec::new())))
    }

    /// Everything written so far. Panics on a real stream — only a test asks.
    #[cfg(test)]
    pub fn contents(&self) -> String {
        match self {
            Stream::Buffer(b) => String::from_utf8_lossy(&b.lock().unwrap()).into_owned(),
            _ => panic!("contents() on a real stream"),
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
            #[cfg(test)]
            Stream::Buffer(_) => false,
        }
    }

    pub(crate) fn write(&self, s: &str) {
        match self {
            Stream::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(s.as_bytes());
                let _ = out.flush();
            }
            Stream::Stderr => {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(s.as_bytes());
                let _ = err.flush();
            }
            #[cfg(test)]
            Stream::Buffer(b) => b.lock().unwrap().extend_from_slice(s.as_bytes()),
        }
    }
}

/// Writes to a pair of streams and knows whether it may use colour.
#[derive(Clone)]
pub struct Ui {
    pub out: Stream,
    pub err: Stream,
    pub color: bool,
}

impl Ui {
    pub fn new(out: Stream, err: Stream, color: bool) -> Ui {
        Ui { out, err, color }
    }

    /// A UI writing to two buffers, for tests.
    #[cfg(test)]
    pub fn buffered() -> Ui {
        Ui::new(Stream::buffer(), Stream::buffer(), false)
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_string();
        }
        format!("{code}{s}{RESET}")
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint(BOLD, s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint(DIM, s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint(RED, s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint(GREEN, s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint(YELLOW, s)
    }
    /// A prefixed status line on stdout. The prefix is what scripts grep for.
    pub fn status(&self, msg: &str) {
        self.out.write(&format!("{PROG}: {msg}\n"));
    }

    /// Success, for the commands where silence would be ambiguous.
    pub fn ok(&self, msg: &str) {
        self.out
            .write(&format!("{PROG}: {} {msg}\n", self.green(GLYPH_OK)));
    }

    /// Advisory: something worth saying that does not change the outcome.
    /// Goes to stderr so it cannot land inside a document on stdout.
    pub fn warn(&self, msg: &str) {
        self.err
            .write(&format!("{PROG}: {} {msg}\n", self.yellow(GLYPH_WARN)));
    }

    /// A failure.
    pub fn error(&self, msg: &str) {
        self.err
            .write(&format!("{PROG}: {} {msg}\n", self.red(GLYPH_ERROR)));
    }

    /// A raw line on stdout, with no prefix — for output a machine reads.
    pub fn println(&self, s: &str) {
        self.out.write(&format!("{s}\n"));
    }

    pub fn blank(&self) {
        self.out.write("\n");
    }

    /// Opens a section: a blank line, a bold title, a blank line. Running a
    /// list straight into the summary line above it is hard to scan once the
    /// list is eighty packages long.
    pub fn header(&self, title: &str) {
        self.out.write(&format!("\n{}\n\n", self.bold(title)));
    }
}

/// Decides whether to colourise, honouring the conventions a user is entitled
/// to expect: an explicit flag beats everything, then `NO_COLOR`, then a dumb
/// terminal, then whether the stream is actually a terminal.
pub fn should_color(mode: ColorMode, stream: &Stream) -> bool {
    match mode {
        // Deliberately above the NO_COLOR check: a user who typed
        // `--color always` has said something more specific than their
        // environment did, most often to pipe coloured output somewhere.
        ColorMode::Always => return true,
        ColorMode::Never => return false,
        ColorMode::Auto => {}
    }
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    match std::env::var("TERM").as_deref() {
        Ok("dumb") | Ok("") | Err(_) => return false,
        _ => {}
    }
    stream.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    use snout_common::testenv::env_guard as guard;

    struct Env;
    impl Env {
        fn set(term: Option<&str>, no_color: bool) -> Env {
            unsafe {
                match term {
                    Some(t) => std::env::set_var("TERM", t),
                    None => std::env::remove_var("TERM"),
                }
                if no_color {
                    std::env::set_var("NO_COLOR", "1");
                } else {
                    std::env::remove_var("NO_COLOR");
                }
            }
            Env
        }
    }

    #[test]
    fn always_beats_no_color() {
        // NO_COLOR is a convention about defaults, not a veto. Someone who
        // typed --color always has been more specific than their environment.
        let _g = guard();
        let _e = Env::set(Some("xterm"), true);
        assert!(should_color(ColorMode::Always, &Stream::buffer()));
    }

    #[test]
    fn never_beats_everything() {
        let _g = guard();
        let _e = Env::set(Some("xterm"), false);
        assert!(!should_color(ColorMode::Never, &Stream::Stdout));
    }

    #[test]
    fn auto_honours_no_color_then_term_then_the_stream() {
        let _g = guard();

        let _e = Env::set(Some("xterm"), true);
        assert!(!should_color(ColorMode::Auto, &Stream::Stdout));

        // A dumb or absent TERM cannot be trusted with escapes even if the
        // stream is a terminal.
        let _e = Env::set(Some("dumb"), false);
        assert!(!should_color(ColorMode::Auto, &Stream::Stdout));
        let _e = Env::set(None, false);
        assert!(!should_color(ColorMode::Auto, &Stream::Stdout));

        // A buffer is never a terminal, which is what makes test output clean.
        let _e = Env::set(Some("xterm"), false);
        assert!(!should_color(ColorMode::Auto, &Stream::buffer()));
    }

    #[test]
    fn nothing_is_painted_when_colour_is_off() {
        let ui = Ui::buffered();
        ui.status("plain");
        ui.ok("fine");
        ui.error("broken");
        let out = ui.out.contents();
        let err = ui.err.contents();
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!err.contains('\x1b'), "{err:?}");
        // The glyphs survive: they are the only thing marking the outcome once
        // the colour is gone.
        assert_eq!(out, "snout: plain\nsnout: ✓ fine\n");
        assert_eq!(err, "snout: ✗ broken\n");
    }

    #[test]
    fn colour_wraps_only_the_glyph() {
        let ui = Ui::new(Stream::buffer(), Stream::buffer(), true);
        ui.ok("built");
        assert_eq!(ui.out.contents(), "snout: \x1b[32m✓\x1b[0m built\n");
    }

    #[test]
    fn painting_an_empty_string_adds_nothing() {
        // Otherwise every blank table cell would carry a pair of escapes that
        // the width calculation then has to subtract back out.
        let ui = Ui::new(Stream::buffer(), Stream::buffer(), true);
        assert_eq!(ui.dim(""), "");
    }

    #[test]
    fn advisories_go_to_stderr_so_they_cannot_land_in_a_document() {
        let ui = Ui::buffered();
        ui.warn("could not refresh the cache");
        assert_eq!(ui.out.contents(), "");
        assert_eq!(ui.err.contents(), "snout: ! could not refresh the cache\n");
    }

    #[test]
    fn println_carries_no_prefix() {
        // `has-updates` prints a bare yes/no for a script to read; a prefix
        // would have to be stripped back off.
        let ui = Ui::buffered();
        ui.println("yes");
        assert_eq!(ui.out.contents(), "yes\n");
    }

    #[test]
    fn a_header_opens_with_a_blank_line() {
        let ui = Ui::buffered();
        ui.header("Packages");
        assert_eq!(ui.out.contents(), "\nPackages\n\n");
    }
}
