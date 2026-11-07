//! A level filter in front of the daemon's log lines.
//!
//! The daemon has a handful of log lines, all of them prose meant for a human
//! reading `journalctl --user -u snout`, and none of them carry structure worth
//! querying on. A structured logger would turn every one of those lines into
//! key=value output to buy filtering nothing needs.
//!
//! The journal supplies its own timestamps, but the logger keeps one of its own
//! so that running the daemon in a terminal — which is how anyone debugs it —
//! still shows when things happened.

use std::io::Write;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

/// Where a line goes. Two variants, because the daemon writes to exactly one
/// place and a test needs to read back what it wrote — the CLI's richer stream
/// type is about colour and TTY detection, neither of which means anything to a
/// process whose output is a journal.
#[derive(Clone)]
pub enum Sink {
    Stderr,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

impl Sink {
    fn write(&self, s: &str) {
        match self {
            Sink::Stderr => {
                let _ = std::io::stderr().write_all(s.as_bytes());
            }
            #[cfg(test)]
            Sink::Buffer(b) => {
                let _ = b.lock().unwrap().write_all(s.as_bytes());
            }
        }
    }
}

/// Ordered so that a message is printed when its level is at or below the
/// configured one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    /// An unrecognised name means info; the config validator rejects those
    /// before they get here, so this is a floor rather than a policy.
    pub fn parse(name: &str) -> Level {
        match name {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "debug" => Level::Debug,
            _ => Level::Info,
        }
    }
}

#[derive(Clone)]
pub struct Logger {
    out: Option<Sink>,
    level: Level,
    /// A clock, so a test can assert on a whole line rather than a substring.
    now: Arc<dyn Fn() -> String + Send + Sync>,
}

impl Logger {
    pub fn new(out: Sink, level: &str) -> Logger {
        Logger {
            out: Some(out),
            level: Level::parse(level),
            now: Arc::new(|| {
                snout_common::time::now()
                    .format("%Y/%m/%d %H:%M:%S")
                    .to_string()
            }),
        }
    }

    /// Drops everything, for tests that are not about logging.
    #[cfg(test)]
    pub fn discard() -> Logger {
        Logger {
            out: None,
            level: Level::Error,
            now: Arc::new(String::new),
        }
    }

    fn log(&self, level: Level, msg: &str) {
        if level > self.level {
            return;
        }
        if let Some(out) = &self.out {
            out.write(&format!("{} {msg}\n", (self.now)()));
        }
    }

    pub fn error(&self, msg: impl AsRef<str>) {
        self.log(Level::Error, msg.as_ref());
    }
    pub fn warn(&self, msg: impl AsRef<str>) {
        self.log(Level::Warn, msg.as_ref());
    }
    pub fn info(&self, msg: impl AsRef<str>) {
        self.log(Level::Info, msg.as_ref());
    }
    pub fn debug(&self, msg: impl AsRef<str>) {
        self.log(Level::Debug, msg.as_ref());
    }
}

/// Collects lines, for tests.
#[cfg(test)]
pub struct Capture(pub Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Capture {
    pub fn logger(level: &str) -> (Logger, Capture) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut l = Logger::new(Sink::Buffer(Arc::clone(&buf)), level);
        l.now = Arc::new(|| "T".to_string());
        (l, Capture(buf))
    }

    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(|l| l.trim_start_matches("T ").to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_prints_itself_and_everything_louder() {
        let cases = [
            ("error", vec!["e"]),
            ("warn", vec!["e", "w"]),
            ("info", vec!["e", "w", "i"]),
            ("debug", vec!["e", "w", "i", "d"]),
            // config::validate rejects this before it gets here; the floor
            // exists so a hand-edited file degrades to info rather than to
            // silence.
            ("chatty", vec!["e", "w", "i"]),
        ];
        for (level, want) in cases {
            let (log, cap) = Capture::logger(level);
            log.error("e");
            log.warn("w");
            log.info("i");
            log.debug("d");
            assert_eq!(cap.lines(), want, "level {level}");
        }
    }

    #[test]
    fn a_discarding_logger_writes_nothing() {
        let log = Logger::discard();
        log.error("this must not appear anywhere");
    }

    #[test]
    fn lines_carry_a_timestamp_for_a_terminal_run() {
        // The journal adds its own, but anyone debugging runs the daemon in a
        // terminal, where a bare message says nothing about when it happened.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let log = Logger::new(Sink::Buffer(Arc::clone(&buf)), "info");
        log.info("watching");
        let text = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(text.ends_with(" watching\n"), "{text:?}");
        assert!(text.len() > "watching\n".len());
    }
}
