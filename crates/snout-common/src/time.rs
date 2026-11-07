//! The clock the cached documents are stamped with.
//!
//! Here rather than beside the CLI's rendering helpers because a timestamp is
//! part of every document that crosses the socket: the daemon writes them, the
//! CLI reads them, and both have to agree on what one is.

use chrono::{DateTime, Local, Timelike};

/// The clock snout stores and compares. Local rather than UTC so a published
/// timestamp carries the offset the machine was in when it was written, which
/// is what a reader comparing it to their own clock needs.
pub type Timestamp = DateTime<Local>;

/// Now, truncated to the second.
///
/// Sub-second precision would survive into every published document and every
/// test expectation without ever being read.
pub fn now() -> Timestamp {
    Local::now()
        .with_nanosecond(0)
        .expect("0ns is always valid")
}

/// Just the clock part, for the "next check 19:52" suffix and the daemon's
/// "next attempt" log line.
pub fn clock(t: Timestamp) -> String {
    t.format("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_just_the_time() {
        use chrono::TimeZone;
        let t = Local.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        assert_eq!(clock(t), "12:00");
    }

    #[test]
    fn now_carries_no_sub_second_precision() {
        // It would survive into every published document and every test
        // expectation without ever being read.
        assert_eq!(now().nanosecond(), 0);
    }
}
