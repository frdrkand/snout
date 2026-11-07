//! Rendering byte counts and timestamps the way the terminal shows them.
//!
//! The timestamp *type* is shared with the daemon; how it reads is not, and
//! nothing here crosses the socket.

use snout_common::time::Timestamp;

/// Renders a byte count with a binary prefix and a sign, matching the shape nix
/// itself uses so the two are comparable at a glance.
pub fn size(n: i64) -> String {
    let (sign, v) = if n < 0 {
        ("-", n.unsigned_abs())
    } else {
        ("", n as u64)
    };
    const UNIT: u64 = 1024;
    if v < UNIT {
        return format!("{sign}{v} B");
    }
    let (mut div, mut exp) = (UNIT, 0usize);
    let mut m = v / UNIT;
    while m >= UNIT && exp < 4 {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    let unit = b"KMGTP"[exp] as char;
    format!("{sign}{:.1} {unit}iB", v as f64 / div as f64)
}

/// The "1 input" / "3 inputs" helper the summary lines need.
pub fn plural(n: usize, singular: &str, plural_form: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural_form}")
    }
}

/// How long ago something happened, which is almost always what the reader
/// wants; the absolute timestamp is kept alongside it for the cases where it is
/// not.
pub fn ago(t: Option<Timestamp>, now: Timestamp) -> String {
    let Some(t) = t else {
        return "never".to_string();
    };
    let d = now.signed_duration_since(t);
    // A negative delta means the stored timestamp is ahead of the clock — a
    // resynced clock, or a file copied from elsewhere. "in 3 minutes" would be
    // stranger to read than "just now".
    if d.num_minutes() < 1 {
        "just now".to_string()
    } else if d.num_hours() < 1 {
        format!(
            "{} ago",
            plural(d.num_minutes() as usize, "minute", "minutes")
        )
    } else if d.num_days() < 1 {
        format!("{} ago", plural(d.num_hours() as usize, "hour", "hours"))
    } else {
        format!("{} ago", plural(d.num_days() as usize, "day", "days"))
    }
}

/// A timestamp as both surfaces show it: local wall time, with the relative
/// form in parentheses.
pub fn when(t: Option<Timestamp>, now: Timestamp) -> String {
    match t {
        None => "never".to_string(),
        Some(t) => format!("{} ({})", t.format("%Y-%m-%d %H:%M"), ago(Some(t), now)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn sizes_match_the_shape_nix_prints() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(8192), "8.0 KiB");
        assert_eq!(size(2_411_724), "2.3 MiB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn a_negative_size_keeps_its_sign() {
        // A closure that shrank is as interesting as one that grew, and the
        // sign is the only thing distinguishing them.
        assert_eq!(size(-855_450), "-835.4 KiB");
        assert_eq!(size(-512), "-512 B");
    }

    #[test]
    fn the_unit_ladder_stops_at_pebibytes() {
        // exp is capped at 4, so an absurd number degrades to a large PiB
        // figure rather than indexing past the end of the unit table.
        assert!(size(i64::MAX).ends_with(" PiB"), "{}", size(i64::MAX));
    }

    #[test]
    fn plural_switches_only_at_one() {
        assert_eq!(plural(0, "input", "inputs"), "0 inputs");
        assert_eq!(plural(1, "input", "inputs"), "1 input");
        assert_eq!(plural(3, "input", "inputs"), "3 inputs");
    }

    /// A fixed instant, built in the *local* zone rather than parsed from one
    /// with a hardcoded offset — these render in local time, so an assertion
    /// written against `+02:00` passes only on a machine that happens to be
    /// there, and fails in a build sandbox running UTC.
    fn base() -> Timestamp {
        use chrono::{Local, TimeZone};
        Local.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
    }

    #[test]
    fn ago_reads_in_the_largest_unit_that_fits() {
        let now = base();
        let cases = [
            (TimeDelta::seconds(0), "just now"),
            (TimeDelta::seconds(59), "just now"),
            (TimeDelta::minutes(1), "1 minute ago"),
            (TimeDelta::minutes(59), "59 minutes ago"),
            (TimeDelta::hours(1), "1 hour ago"),
            (TimeDelta::hours(23), "23 hours ago"),
            (TimeDelta::hours(24), "1 day ago"),
            (TimeDelta::days(9), "9 days ago"),
        ];
        for (delta, want) in cases {
            assert_eq!(ago(Some(now - delta), now), want, "delta {delta:?}");
        }
    }

    #[test]
    fn a_timestamp_from_the_future_reads_as_just_now() {
        // A resynced clock or a cache copied from another machine. "in 3
        // minutes" would be stranger to read than admitting nothing is known.
        let now = base();
        assert_eq!(ago(Some(now + TimeDelta::hours(3)), now), "just now");
    }

    #[test]
    fn nothing_checked_yet_reads_as_never() {
        assert_eq!(ago(None, base()), "never");
        assert_eq!(when(None, base()), "never");
    }

    #[test]
    fn when_pairs_the_absolute_form_with_the_relative_one() {
        let now = base();
        let t = now - TimeDelta::hours(2);
        assert_eq!(when(Some(t), now), "2026-08-18 10:00 (2 hours ago)");
    }
}
