//! How `nix store diff-closures` decides that two store paths are "the same
//! package at different versions".
//!
//! Ported from nix itself — `src/libstore/names.cc` (`DrvName`) and
//! `src/nix/diff-closures.cc` (`getClosureInfo`) — rather than reinvented,
//! because any divergence shows up as a package silently missing from, or
//! duplicated in, the update list.

/// Removes the 32-character hash prefix from a store path basename, returning
/// the name part. `None` for anything not shaped like a store path basename.
pub fn split_store_path_name(base: &str) -> Option<&str> {
    const HASH_LEN: usize = 32;
    if base.len() < HASH_LEN + 2 || base.as_bytes()[HASH_LEN] != b'-' {
        return None;
    }
    Some(&base[HASH_LEN + 1..])
}

/// Removes a trailing output suffix, mirroring nix's
///
/// ```text
/// static std::regex regex("(.*)-([a-z]+|lib32|lib64)");
/// ```
///
/// matched against the whole name. Two properties of that regex matter and are
/// easy to lose in translation:
///
/// - it is a *full* match, so the suffix has to run to the end of the string —
///   `bash-interactive-5.3p15` is not stripped, because `5.3p15` is not
///   `[a-z]+` and neither is `interactive-5.3p15`;
/// - `.*` is greedy, so when several splits would match, the *rightmost* one
///   wins.
///
/// nix's own comment notes this is ambiguous — it cannot tell the output `bin`
/// from a version suffix like `unstable` — and we inherit that ambiguity on
/// purpose, since the goal is to group exactly as diff-closures groups.
pub fn strip_output_name(name: &str) -> (&str, &str) {
    for (i, b) in name.bytes().enumerate().rev() {
        if b != b'-' {
            continue;
        }
        let suffix = &name[i + 1..];
        if is_output_suffix(suffix) {
            return (&name[..i], suffix);
        }
    }
    (name, "")
}

fn is_output_suffix(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s == "lib32" || s == "lib64" {
        return true;
    }
    s.bytes().all(|b| b.is_ascii_lowercase())
}

/// nix's `DrvName`: the name is everything up to but not including the first
/// dash *not* followed by a letter; the version is the rest. So
/// `apache-httpd-2.0.48` gives `("apache-httpd", "2.0.48")`, and `hwdb.bin`
/// gives `("hwdb.bin", "")`.
pub fn split_name_version(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'-' && i + 1 < b.len() && !b[i + 1].is_ascii_alphabetic() {
            return (&s[..i], &s[i + 1..]);
        }
    }
    (s, "")
}

/// Resolves a store path basename to the `(name, version)` pair diff-closures
/// would group it under.
pub fn package_of(base: &str) -> Option<(&str, &str)> {
    let n = split_store_path_name(base)?;
    let (stripped, _output) = strip_output_name(n);
    Some(split_name_version(stripped))
}

/// Renders a version set the way diff-closures does: the empty set is `∅`, an
/// empty version string is `ε`, and the rest are comma-joined in sorted order.
pub fn show_versions(sorted: &[String]) -> String {
    if sorted.is_empty() {
        return "∅".to_string();
    }
    sorted
        .iter()
        .map(|v| if v.is_empty() { "ε" } else { v.as_str() })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-character hash, so a basename can be written without counting.
    fn path(name: &str) -> String {
        format!("{}-{name}", "a".repeat(32))
    }

    #[test]
    fn a_store_path_basename_loses_its_hash() {
        assert_eq!(
            split_store_path_name(&path("hello-2.12")),
            Some("hello-2.12")
        );
    }

    #[test]
    fn anything_not_shaped_like_a_store_path_is_rejected() {
        // Too short, and no dash where the hash must end. Rejecting rather than
        // guessing is what keeps a stray file in the closure from being counted
        // as a package.
        assert_eq!(split_store_path_name("short"), None);
        assert_eq!(split_store_path_name(&"a".repeat(40)), None);
        assert_eq!(split_store_path_name(""), None);
    }

    #[test]
    fn output_suffixes_are_stripped_only_when_they_run_to_the_end() {
        // The regex is a full match: "interactive-5.3p15" is not [a-z]+, so
        // nothing is stripped and the version survives.
        assert_eq!(
            strip_output_name("bash-interactive-5.3p15"),
            ("bash-interactive-5.3p15", "")
        );
        assert_eq!(
            strip_output_name("libressl-4.3.2-man"),
            ("libressl-4.3.2", "man")
        );
        assert_eq!(
            strip_output_name("openssl-3.0-lib32"),
            ("openssl-3.0", "lib32")
        );
        assert_eq!(
            strip_output_name("openssl-3.0-lib64"),
            ("openssl-3.0", "lib64")
        );
    }

    #[test]
    fn the_rightmost_valid_split_wins() {
        // `.*` is greedy, so of the two candidate splits in "foo-bin-dev" the
        // later one is taken. Matching leftmost instead would strip "bin-dev"
        // and group this under "foo" — a different package.
        assert_eq!(strip_output_name("foo-bin-dev"), ("foo-bin", "dev"));
    }

    #[test]
    fn a_name_with_no_output_suffix_is_left_alone() {
        assert_eq!(strip_output_name("firmware"), ("firmware", ""));
        assert_eq!(strip_output_name("hwdb.bin"), ("hwdb.bin", ""));
    }

    #[test]
    fn the_version_starts_at_the_first_dash_not_followed_by_a_letter() {
        assert_eq!(
            split_name_version("apache-httpd-2.0.48"),
            ("apache-httpd", "2.0.48")
        );
        assert_eq!(split_name_version("hello-2.12"), ("hello", "2.12"));
        // A dash followed by a letter is part of the name, however many there
        // are — otherwise every hyphenated package would be truncated.
        assert_eq!(
            split_name_version("bash-interactive"),
            ("bash-interactive", "")
        );
        assert_eq!(split_name_version("hwdb.bin"), ("hwdb.bin", ""));
        // A version may itself contain dashes; only the first split matters.
        assert_eq!(split_name_version("glibc-2.42-67"), ("glibc", "2.42-67"));
    }

    #[test]
    fn a_trailing_dash_is_not_a_version_boundary() {
        // i+1 < len guards this: "foo-" has no character after the dash to
        // classify, so the whole string is the name.
        assert_eq!(split_name_version("foo-"), ("foo-", ""));
    }

    #[test]
    fn package_of_walks_all_three_steps() {
        // These are the shapes a real closure actually contains, and each one
        // exercises a different branch of the three functions above.
        let cases = [
            ("bash-interactive-5.3p15", "bash-interactive", "5.3p15"),
            ("hwdb.bin", "hwdb.bin", ""),
            ("glibc-2.42-67", "glibc", "2.42-67"),
            ("libressl-4.3.2-man", "libressl", "4.3.2"),
            ("firmware", "firmware", ""),
            (
                "python3.13-requests-2.32.3",
                "python3.13-requests",
                "2.32.3",
            ),
            ("gcc-14.3.0-lib", "gcc", "14.3.0"),
            (
                "nixos-system-somehost-26.11.20260813",
                "nixos-system-somehost",
                "26.11.20260813",
            ),
        ];
        for (base, name, version) in cases {
            assert_eq!(
                package_of(&path(base)),
                Some((name, version)),
                "basename {base}"
            );
        }
    }

    #[test]
    fn package_of_rejects_a_non_store_path() {
        assert_eq!(package_of("not-a-store-path"), None);
    }

    #[test]
    fn version_sets_render_the_way_diff_closures_renders_them() {
        assert_eq!(show_versions(&[]), "∅");
        assert_eq!(show_versions(&["".to_string()]), "ε");
        assert_eq!(show_versions(&["1.0".to_string()]), "1.0");
        assert_eq!(
            show_versions(&["1.0".to_string(), "2.0".to_string()]),
            "1.0, 2.0"
        );
        // A package present with both a version and no version at all — two
        // store paths grouped under one name — must show both.
        assert_eq!(
            show_versions(&["".to_string(), "2.0".to_string()]),
            "ε, 2.0"
        );
    }
}
