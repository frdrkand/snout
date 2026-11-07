//! Trimming nix's stderr down to the part worth reading.
//!
//! Nix reports a failed fetch as a warning followed by the server's entire
//! response — for a GitHub 504 that is several hundred lines of HTML, and it
//! lands in the middle of a check the user is watching:
//!
//! ```text
//! warning: unable to download 'https://api.github.com/…/commits/HEAD': HTTP error 504
//! response body:
//! <html><head><title>Server Error</title>…
//! ```
//!
//! The warning is the diagnosis and is kept. The body says nothing snout's
//! reader can act on, so it is counted and dropped.

/// How many lines of a body to swallow before giving up and letting the rest
/// through. A body that matches no terminator must not be able to eat the
/// remainder of the run.
const MAX_BODY_LINES: usize = 500;

/// A line filter over one command's stderr.
///
/// Stateful, because a body is recognised by the line that opens it and only
/// ends several hundred lines later — so it holds one command's worth of
/// position and is not shared between commands.
#[derive(Debug, Default)]
pub struct Filter {
    /// Lines dropped since the body opened; `None` outside a body.
    body: Option<usize>,
}

impl Filter {
    /// The lines worth forwarding, given one line of nix's stderr.
    pub fn push(&mut self, line: &str) -> Vec<String> {
        let trimmed = line.trim();

        if let Some(dropped) = self.body {
            // `</html>` belongs to the body: consume it and close, rather than
            // ending on the next real message and letting the tag through.
            if trimmed == "</html>" {
                return self.close();
            }
            if starts_a_message(trimmed) {
                let mut out = self.close();
                out.extend(self.push(line));
                return out;
            }
            if dropped >= MAX_BODY_LINES {
                let mut out = self.close();
                out.push(line.to_string());
                return out;
            }
            self.body = Some(dropped + 1);
            return Vec::new();
        }

        if trimmed == "response body:" {
            self.body = Some(0);
            return Vec::new();
        }
        // Blank lines carry nothing and would otherwise punch holes in the
        // spinner's line.
        if trimmed.is_empty() {
            return Vec::new();
        }
        vec![line.to_string()]
    }

    /// Flushes a body that ran to the end of the stream, which is what happens
    /// when the response is the last thing nix printed.
    pub fn finish(&mut self) -> Vec<String> {
        self.close()
    }

    /// Leaves body mode, reporting what was dropped.
    ///
    /// Said out loud rather than silently: a check that swallows several
    /// hundred lines without admitting it is harder to debug than one that
    /// prints them.
    fn close(&mut self) -> Vec<String> {
        match self.body.take() {
            None | Some(0) => Vec::new(),
            Some(1) => vec!["  … response body suppressed (1 line)".to_string()],
            Some(n) => vec![format!("  … response body suppressed ({n} lines)")],
        }
    }
}

/// Whether a line opens a new nix message, and therefore ends whatever body
/// preceded it: a diagnostic prefix, or one of `nix flake update`'s bullets.
fn starts_a_message(trimmed: &str) -> bool {
    trimmed.starts_with('•')
        || ["error:", "warning:", "note:", "hint:", "trace:"]
            .iter()
            .any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `push` kept, for a whole stderr stream.
    fn run(lines: &[&str]) -> Vec<String> {
        let mut f = Filter::default();
        let mut out: Vec<String> = lines.iter().flat_map(|l| f.push(l)).collect();
        out.extend(f.finish());
        out
    }

    #[test]
    fn ordinary_output_passes_through_untouched() {
        assert_eq!(
            run(&["• Updated input 'nixpkgs':", "warning: dirty tree"]),
            ["• Updated input 'nixpkgs':", "warning: dirty tree"]
        );
    }

    #[test]
    fn blank_lines_are_dropped() {
        // The behaviour the checker had before this filter existed: a blank
        // line lands on top of the spinner and says nothing.
        assert_eq!(run(&["one", "", "   ", "two"]), ["one", "two"]);
    }

    #[test]
    fn a_504_keeps_its_warning_and_loses_its_body() {
        let out = run(&[
            "warning: unable to download 'https://api.github.com/repos/vic/import-tree/commits/HEAD': HTTP error 504",
            "response body:",
            "<html>",
            "<head><title>Server Error</title></head>",
            "<body><h1>504</h1></body>",
            "</html>",
            "• Updated input 'nixpkgs':",
        ]);
        assert_eq!(
            out,
            [
                "warning: unable to download 'https://api.github.com/repos/vic/import-tree/commits/HEAD': HTTP error 504",
                "  … response body suppressed (3 lines)",
                "• Updated input 'nixpkgs':",
            ]
        );
    }

    #[test]
    fn a_body_ends_at_the_next_message_when_there_is_no_closing_tag() {
        // Not every body is HTML — an API can answer with JSON or plain text,
        // and the next diagnostic is then the only boundary there is.
        assert_eq!(
            run(&[
                "warning: unable to download 'x': HTTP error 429",
                "response body:",
                "{\"message\":\"rate limited\"}",
                "error: could not fetch input",
            ]),
            [
                "warning: unable to download 'x': HTTP error 429",
                "  … response body suppressed (1 line)",
                "error: could not fetch input",
            ]
        );
    }

    #[test]
    fn a_body_that_runs_to_the_end_is_still_reported() {
        assert_eq!(
            run(&["response body:", "junk", "junk"]),
            ["  … response body suppressed (2 lines)"]
        );
    }

    #[test]
    fn an_empty_body_says_nothing() {
        assert_eq!(
            run(&["response body:", "</html>", "note: done"]),
            ["note: done"]
        );
    }

    #[test]
    fn a_body_that_never_ends_stops_swallowing() {
        // A terminator snout does not recognise must cost a bounded number of
        // lines, not the rest of the build log.
        let mut lines = vec!["response body:"];
        lines.extend(std::iter::repeat_n("x", MAX_BODY_LINES + 10));
        let out = run(&lines);
        assert_eq!(out.len(), 11, "{out:?}");
        assert!(out[0].contains("suppressed"), "{out:?}");
        assert!(out[1..].iter().all(|l| l == "x"), "{out:?}");
    }
}
