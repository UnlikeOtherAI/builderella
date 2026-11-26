//! Problems found in a recipe file, with their position in the text.

use serde::{Deserialize, Serialize};
use std::fmt;

/// How serious an [`Issue`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The file or the requested job cannot be used.
    Error,
    /// Usable, but probably not what the author meant.
    Warning,
}

/// One problem in a recipe file or in a request to resolve one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    /// What is wrong, naming the offending text. One line, no trailing
    /// period.
    pub message: String,
    /// Where the problem is, as a key path: `recipes.test.steps[1].needs[0]`.
    /// Empty for problems with the file as a whole. Paths starting with
    /// `overrides.` and `limits.` refer to the arguments of
    /// [`resolve`](crate::resolve) rather than to the file.
    pub path: String,
    /// 1-based line in the recipe text, when the parser gave a position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column, counted in characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

impl Issue {
    /// `true` for an error, `false` for a warning.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    pub(crate) fn new(
        severity: Severity,
        path: impl Into<String>,
        pos: Option<Pos>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            message: message.into(),
            path: path.into(),
            line: pos.map(|p| p.line),
            column: pos.map(|p| p.column),
        }
    }
}

impl fmt::Display for Issue {
    /// `error: 12:8: recipes.test.memory: message`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.severity {
            Severity::Error => "error: ",
            Severity::Warning => "warning: ",
        })?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, "{line}:{column}: ")?;
        }
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

/// A 1-based position in the recipe text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    pub line: u32,
    pub column: u32,
}

/// Maps byte offsets to line and column.
pub(crate) struct LineIndex<'t> {
    text: &'t str,
    /// Byte offset at which each line starts. A byte-order mark at the
    /// start of the file is not part of the first line.
    starts: Vec<usize>,
}

impl<'t> LineIndex<'t> {
    pub fn new(text: &'t str) -> Self {
        let mut starts = vec![if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        }];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { text, starts }
    }

    pub fn pos(&self, offset: usize) -> Pos {
        let offset = offset.clamp(self.starts[0], self.text.len());
        let line = self.starts.partition_point(|start| *start <= offset) - 1;
        let start = self.starts[line];
        // An offset inside a multi-byte character cannot come from the
        // parser; fall back to counting bytes rather than failing.
        let column = match self.text.get(start..offset) {
            Some(prefix) => prefix.chars().count(),
            None => offset - start,
        };
        Pos {
            line: u32::try_from(line + 1).unwrap_or(u32::MAX),
            column: u32::try_from(column + 1).unwrap_or(u32::MAX),
        }
    }
}

/// Order issues the way a reader meets them: by position, then errors first.
pub(crate) fn sort_issues(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        let key = |i: &Issue| {
            (
                i.line.unwrap_or(u32::MAX),
                i.column.unwrap_or(u32::MAX),
                i.severity != Severity::Error,
            )
        };
        key(a).cmp(&key(b))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_one_based_and_count_characters() {
        let text = "a = 1\nžluť = \"x\"\n";
        let index = LineIndex::new(text);
        assert_eq!(index.pos(0), Pos { line: 1, column: 1 });
        assert_eq!(index.pos(4), Pos { line: 1, column: 5 });
        let value = text.find('"').unwrap();
        // Four characters and " = " precede the quote, though `ž` and `ť`
        // take two bytes each.
        assert_eq!(index.pos(value), Pos { line: 2, column: 8 });
        assert_eq!(index.pos(text.len()), Pos { line: 3, column: 1 });
        assert_eq!(index.pos(text.len() + 50), Pos { line: 3, column: 1 });
    }

    #[test]
    fn a_byte_order_mark_and_carriage_returns_do_not_shift_positions() {
        let text = "\u{feff}a = 1\r\nb = 2\r\n";
        let index = LineIndex::new(text);
        assert_eq!(
            index.pos(text.find('a').unwrap()),
            Pos { line: 1, column: 1 }
        );
        assert_eq!(
            index.pos(text.find('1').unwrap()),
            Pos { line: 1, column: 5 }
        );
        assert_eq!(
            index.pos(text.find('b').unwrap()),
            Pos { line: 2, column: 1 }
        );
        assert_eq!(
            index.pos(text.find('2').unwrap()),
            Pos { line: 2, column: 5 }
        );
        // An offset inside the mark is reported as the start of the file.
        assert_eq!(index.pos(0), Pos { line: 1, column: 1 });
        assert_eq!(LineIndex::new("").pos(0), Pos { line: 1, column: 1 });
    }

    #[test]
    fn display_is_compact() {
        let issue = Issue::new(
            Severity::Error,
            "recipes.test.memory",
            Some(Pos {
                line: 12,
                column: 8,
            }),
            "bad",
        );
        assert_eq!(issue.to_string(), "error: 12:8: recipes.test.memory: bad");
        let bare = Issue::new(Severity::Warning, "", None, "odd");
        assert_eq!(bare.to_string(), "warning: odd");
    }
}
