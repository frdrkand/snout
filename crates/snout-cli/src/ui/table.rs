//! The aligned two-or-three column list both `list-updates` and `diff` print.

use super::Ui;

/// How the destination column is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// A version that moved, or one that appeared: both are green, because
    /// both are something arriving.
    #[default]
    Arriving,
    /// A version that went away.
    Removed,
}

/// One line of a two-or-three column list.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub name: String,
    pub from: String,
    pub to: String,
    pub note: String,
    pub style: Style,
}

impl Row {
    pub fn new(name: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Row {
        Row {
            name: name.into(),
            from: from.into(),
            to: to.into(),
            ..Default::default()
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Row {
        self.note = note.into();
        self
    }

    pub fn style(mut self, style: Style) -> Row {
        self.style = style;
        self
    }
}

impl Ui {
    /// Prints aligned rows.
    ///
    /// Alignment is computed from the widest entry in each column, so the
    /// arrows and the sizes line up and the eye can run down the column — which
    /// is the whole reason this is not just a series of formatted prints.
    pub fn table(&self, rows: &[Row]) {
        if rows.is_empty() {
            return;
        }
        let name_w = rows.iter().map(|r| width(&r.name)).max().unwrap_or(0);
        let from_w = rows.iter().map(|r| width(&r.from)).max().unwrap_or(0);
        let note_w = rows.iter().map(|r| width(&r.note)).max().unwrap_or(0);
        // Only measured when there is a note column to align, so a table
        // without one does not grow a trailing run of spaces on every line.
        let to_w = if note_w > 0 {
            rows.iter().map(|r| width(&r.to)).max().unwrap_or(0)
        } else {
            0
        };

        for r in rows {
            let painted_to = match r.style {
                Style::Arriving => self.green(&r.to),
                Style::Removed => self.red(&r.to),
            };
            let mut line = format!(
                "  {}  {} {} {}",
                pad(&self.bold(&r.name), &r.name, name_w),
                pad(&self.dim(&r.from), &r.from, from_w),
                self.dim("→"),
                pad(&painted_to, &r.to, to_w),
            );
            if !r.note.is_empty() {
                line.push_str("  ");
                // Right-aligned: the notes are sizes, and a size column reads
                // as a column only when the digits end in the same place.
                line.push_str(&lpad(&self.dim(&r.note), &r.note, note_w));
            }
            // A row with no note still padded `to`, so trim what it does not
            // need rather than printing a line of trailing blanks.
            line.truncate(line.trim_end().len());
            line.push('\n');
            self.out.write(&line);
        }
    }
}

/// Appends spaces based on the *visible* width, since the painted string
/// carries escape sequences that occupy no columns.
fn pad(painted: &str, plain: &str, w: usize) -> String {
    format!("{painted}{}", " ".repeat(w.saturating_sub(width(plain))))
}

/// [`pad`], from the other side.
fn lpad(painted: &str, plain: &str, w: usize) -> String {
    format!("{}{painted}", " ".repeat(w.saturating_sub(width(plain))))
}

/// Counts characters rather than bytes, so the `∅` and `ε` nix uses for
/// one-sided packages do not throw the alignment out — each is three bytes and
/// one column.
fn width(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Stream;

    fn lines(ui: &Ui) -> Vec<String> {
        ui.out.contents().lines().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_table_prints_nothing() {
        // Not even a blank line: the caller decides whether an empty list is
        // worth mentioning, and usually says so in the summary instead.
        let ui = Ui::buffered();
        ui.table(&[]);
        assert_eq!(ui.out.contents(), "");
    }

    #[test]
    fn columns_are_padded_to_the_widest_entry() {
        let ui = Ui::buffered();
        ui.table(&[
            Row::new("nixpkgs", "0e251e2", "e5bdc4a"),
            Row::new("wrappers", "6e7f66f", "7472616"),
        ]);
        assert_eq!(
            lines(&ui),
            [
                "  nixpkgs   0e251e2 → e5bdc4a",
                "  wrappers  6e7f66f → 7472616",
            ]
        );
    }

    #[test]
    fn alignment_is_measured_in_characters_not_bytes() {
        // ∅ is three bytes and one column. Measuring bytes would push every
        // other row two spaces too far right, which is exactly the misalignment
        // the table exists to prevent.
        let ui = Ui::buffered();
        ui.table(&[
            Row::new("added-pkg", "∅", "1.0"),
            Row::new("x", "1.0", "2.0"),
        ]);
        assert_eq!(
            lines(&ui),
            ["  added-pkg  ∅   → 1.0", "  x          1.0 → 2.0"]
        );
    }

    #[test]
    fn a_note_is_appended_dimmed() {
        let ui = Ui::buffered();
        ui.table(&[Row::new("bluez", "5.86", "5.87").note("-1.1 MiB")]);
        assert_eq!(lines(&ui), ["  bluez  5.86 → 5.87  -1.1 MiB"]);
    }

    #[test]
    fn notes_line_up_under_each_other() {
        // The destination column is what varies — versions are not one width —
        // so leaving it unpadded is what made the size column ragged. The notes
        // are right-aligned because they are sizes, and a size reads as a
        // column only when its last digit does.
        let ui = Ui::buffered();
        ui.table(&[
            Row::new("at-spi2-core", "2.60.4", "2.60.6"),
            Row::new("bluez", "5.86", "5.87").note("-1.1 MiB"),
            Row::new("firefox", "140.0.1", "141.0").note("32.0 MiB"),
        ]);
        assert_eq!(
            lines(&ui),
            [
                "  at-spi2-core  2.60.4  → 2.60.6",
                "  bluez         5.86    → 5.87    -1.1 MiB",
                "  firefox       140.0.1 → 141.0   32.0 MiB",
            ]
        );
    }

    #[test]
    fn a_row_without_a_note_carries_no_trailing_blanks() {
        // The destination column is padded for the rows that do have one, and
        // an invisible run of spaces at the end of a line is the kind of thing
        // that shows up in a diff long after anyone remembers why.
        let ui = Ui::buffered();
        ui.table(&[
            Row::new("short", "1.0", "2.0"),
            Row::new("long", "1.0", "2.0.0.0").note("8.0 KiB"),
        ]);
        for line in lines(&ui) {
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    #[test]
    fn only_a_removal_is_painted_red() {
        // Both a version move and a new package are green: each is something
        // arriving, and distinguishing them by colour would say nothing the
        // ∅ in the `from` column does not already say.
        let ui = Ui::new(Stream::buffer(), Stream::buffer(), true);
        ui.table(&[
            Row::new("moved", "1.0", "2.0"),
            Row::new("gained", "∅", "1.0"),
            Row::new("lost", "1.0", "∅").style(Style::Removed),
        ]);
        let out = ui.out.contents();
        assert_eq!(out.matches("\x1b[32m").count(), 2, "{out:?}");
        assert_eq!(out.matches("\x1b[31m").count(), 1, "{out:?}");
    }

    #[test]
    fn escapes_do_not_count_toward_the_column_width() {
        // The painted string is longer in bytes than the plain one; padding on
        // the painted length would produce a table that only lines up when
        // colour is off.
        let plain = Ui::buffered();
        let painted = Ui::new(Stream::buffer(), Stream::buffer(), true);
        let rows = [Row::new("a", "1", "2"), Row::new("long-name", "1", "2")];
        plain.table(&rows);
        painted.table(&rows);

        let strip = |s: String| {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(strip(painted.out.contents()), plain.out.contents());
    }
}
