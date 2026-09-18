//! Bounded preparation for accepted single-range reads, independent of line
//! density.

use std::collections::VecDeque;

use super::{LineEndingKind, ReadLineRange, render_line_into};
use crate::shell_output_spool::MAX_SAVED_OUTPUT_BYTES;
use crate::truncate::{
    TRUNCATED_OUTPUT_HEAD_LINES, TRUNCATED_OUTPUT_TAIL_LINES, Truncated,
    truncate_line_oriented_lines,
};

#[cfg(test)]
mod tests;

/// A selected line borrowing the input rather than owning decoded content.
struct SourceLine<'a> {
    /// Original one-based source line number.
    number: usize,
    /// Source bytes excluding the line delimiter.
    bytes: &'a [u8],
    /// Exact delimiter, or absence of a final newline.
    ending: Option<LineEndingKind>,
}

impl SourceLine<'_> {
    fn render_into(&self, output: &mut String) {
        let content = String::from_utf8_lossy(self.bytes);
        render_line_into(
            output,
            self.number,
            &content,
            matches!(content, std::borrow::Cow::Owned(_)),
            self.ending,
        );
    }
}

/// Only the prefix and head/tail descriptors needed by the existing truncator.
pub(super) struct Prepared<'a> {
    /// Source-wide count, including lines outside the requested range.
    pub(super) total_lines: usize,
    /// Source-wide UTF-8 validity, including unselected bytes.
    pub(super) valid_utf8: bool,
    /// Exact byte length of the full numbered selected rendering.
    pub(super) rendered_bytes: usize,
    /// UTF-8-safe prefix of the full rendering, bounded by the saved-output
    /// cap.
    pub(super) prefix: String,
    /// Number of selected source lines before candidate reduction.
    selected_lines: usize,
    /// First selected lines; never exceeds the shared head budget.
    head: Vec<SourceLine<'a>>,
    /// Subsequent selected lines, retaining only the shared tail budget.
    tail: VecDeque<SourceLine<'a>>,
}

impl<'a> Prepared<'a> {
    /// Scan the full bounded source while retaining only visible candidates and
    /// a prefix.
    pub(super) fn new(input: &'a [u8], range: &ReadLineRange) -> Self {
        let mut prepared = Self {
            total_lines: 0,
            valid_utf8: std::str::from_utf8(input).is_ok(),
            rendered_bytes: 0,
            prefix: String::new(),
            selected_lines: 0,
            head: Vec::new(),
            tail: VecDeque::new(),
        };
        let mut scratch = String::new();
        let mut start = 0;
        let mut index = 0;
        while index < input.len() {
            let (ending, delimiter_bytes) = match input[index] {
                b'\r' if input.get(index + 1) == Some(&b'\n') => (LineEndingKind::Crlf, 2),
                b'\r' => (LineEndingKind::Cr, 1),
                b'\n' => (LineEndingKind::Lf, 1),
                _ => {
                    index += 1;
                    continue;
                }
            };
            prepared.push(&input[start..index], Some(ending), range, &mut scratch);
            index += delimiter_bytes;
            start = index;
        }
        if start < input.len() {
            prepared.push(&input[start..], None, range, &mut scratch);
        }
        prepared
    }

    fn push(
        &mut self,
        bytes: &'a [u8],
        ending: Option<LineEndingKind>,
        range: &ReadLineRange,
        scratch: &mut String,
    ) {
        self.total_lines += 1;
        if !range.contains_line(self.total_lines) {
            return;
        }
        let line = SourceLine {
            number: self.total_lines,
            bytes,
            ending,
        };
        line.render_into(scratch);
        if self.selected_lines != 0 {
            self.append_prefix("\n");
        }
        self.append_prefix(scratch);
        self.selected_lines += 1;
        if self.head.len() < TRUNCATED_OUTPUT_HEAD_LINES {
            self.head.push(line);
        } else {
            if self.tail.len() == TRUNCATED_OUTPUT_TAIL_LINES {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
    }

    fn append_prefix(&mut self, text: &str) {
        // Once any bytes were omitted, later pieces must not fill a UTF-8 gap.
        if self.prefix.len() == self.rendered_bytes {
            let mut end = text.len().min(MAX_SAVED_OUTPUT_BYTES - self.prefix.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.prefix.push_str(&text[..end]);
        }
        self.rendered_bytes += text.len();
    }

    /// Apply the shared visible-output policy with the original selected
    /// totals.
    pub(super) fn truncate(&self) -> Truncated {
        let rendered = self
            .head
            .iter()
            .chain(self.tail.iter())
            .map(|line| {
                let mut text = String::new();
                line.render_into(&mut text);
                text
            })
            .collect::<Vec<_>>();
        truncate_line_oriented_lines(
            rendered.iter().map(String::as_str),
            self.selected_lines,
            self.rendered_bytes,
        )
    }
}
