//! Dual-buffer diff-based screen renderer.
//!
//! Maintains an "actual" buffer representing what is currently on the
//! terminal and diffs it against a "desired" buffer to emit only the
//! escape sequences needed to update changed characters. This minimizes
//! terminal I/O, which matters over slow SSH connections.
//!
//! Approach borrowed from fish shell's `screen.rs`:
//! <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//!
//! Key differences from fish:
//! - We don't track styling/attributes (yet), just character content.
//! - We use a simpler line model (Vec<Vec<char>>) instead of fish's
//!   Line struct with soft-wrap tracking.
//! - We always use relative cursor movement (MoveUp, `\r`, `\n`,
//!   MoveToColumn) — never absolute positioning.
//!
//! Downward movement uses `\n` rather than `MoveDown` because `\n`
//! scrolls the terminal when at the bottom of the screen and creates
//! new physical lines, while `MoveDown` silently stops at the edge.

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::QueueableCommand;

/// Virtual screen state with diff-based updates.
pub struct Screen {
    /// What we believe is currently displayed on the terminal.
    lines: Vec<Vec<char>>,
    /// Current terminal cursor row (relative to prompt start).
    cursor_row: usize,
    /// Current terminal cursor column.
    cursor_col: usize,
    /// Terminal width in columns.
    width: usize,
}

impl Screen {
    pub fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            width: width.max(1),
        }
    }

    /// Updates the terminal width. Call after a resize.
    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(1);
    }

    /// Returns the current terminal width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Diffs the desired content against the actual screen state and emits
    /// only the escape sequences needed to make the terminal match.
    ///
    /// `desired_lines` is the content split into physical rows.
    /// `desired_cursor` is `(row, col)` where the cursor should end up.
    pub fn update(
        &mut self,
        w: &mut impl Write,
        desired_lines: &[Vec<char>],
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        // Handle empty desired.
        if desired_lines.is_empty() {
            if !self.lines.is_empty() {
                self.move_to(w, 0, 0)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            }
            self.lines.clear();
            self.cursor_row = 0;
            self.cursor_col = 0;
            w.flush()?;
            return Ok(());
        }

        let desired_count = desired_lines.len();

        for row in 0..desired_count {
            let actual_line = self.lines.get(row);
            let actual_slice = actual_line.map(|l| l.as_slice()).unwrap_or(&[]);
            let desired_slice = desired_lines[row].as_slice();

            // Find the first column where actual and desired differ.
            let common_prefix = actual_slice
                .iter()
                .zip(desired_slice.iter())
                .take_while(|(a, d)| a == d)
                .count();

            let is_last_desired = row == desired_count - 1;
            let actual_longer = actual_slice.len() > desired_slice.len();
            let has_extra_actual_below = is_last_desired && self.lines.len() > desired_count;

            // Skip if this line is completely unchanged and we don't need
            // to clear below.
            if common_prefix == actual_slice.len()
                && common_prefix == desired_slice.len()
                && !has_extra_actual_below
            {
                continue;
            }

            // Move to the first changed column on this row.
            self.move_to(w, row, common_prefix)?;

            // Print the new content from the first difference onward.
            if common_prefix < desired_slice.len() {
                let new_content: String = desired_slice[common_prefix..].iter().collect();
                w.queue(Print(new_content))?;
                let new_col = desired_slice.len();
                if new_col >= self.width {
                    // Printed a full line — the terminal wraps the cursor
                    // to column 0 of the next row.
                    self.cursor_row += new_col / self.width;
                    self.cursor_col = new_col % self.width;
                } else {
                    self.cursor_col = new_col;
                }
            }

            // Clear trailing characters / lines below as needed.
            if has_extra_actual_below {
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_longer {
                w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
        }

        // Position the cursor where it should be.
        self.move_to(w, desired_cursor.0, desired_cursor.1)?;

        w.flush()?;

        // Actual now matches desired.
        self.lines = desired_lines.to_vec();

        Ok(())
    }

    /// Resets the actual state to empty. Call this after externally
    /// clearing the prompt area (e.g. before printing async output).
    /// The next `update()` will treat everything as new.
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Moves the cursor to the top of the prompt area and clears
    /// everything from there down. After this, `invalidate()` should
    /// be called to reset the actual state.
    pub fn erase_all(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.cursor_row > 0 {
            w.queue(MoveUp(self.cursor_row as u16))?;
        }
        w.queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::FromCursorDown))?;
        self.cursor_row = 0;
        self.cursor_col = 0;
        Ok(())
    }

    /// Number of physical lines currently tracked as on-screen.
    pub fn actual_line_count(&self) -> usize {
        self.lines.len()
    }

    /// Moves the terminal cursor from the current position to `(row, col)`
    /// using relative movement.
    ///
    /// Uses `\n` for downward movement (scrolls at screen bottom, creates
    /// lines) and `MoveUp` for upward movement. Column is set with
    /// `MoveToColumn` after vertical movement.
    fn move_to(&mut self, w: &mut impl Write, row: usize, col: usize) -> io::Result<()> {
        // Vertical movement.
        if row < self.cursor_row {
            w.queue(MoveUp((self.cursor_row - row) as u16))?;
        } else if row > self.cursor_row {
            // Use \n for downward movement — it scrolls the terminal
            // when at the bottom, unlike MoveDown which silently stops.
            let down = row - self.cursor_row;
            for _ in 0..down {
                w.queue(Print("\n"))?;
            }
            // \n may or may not reset column depending on terminal and
            // raw mode settings, so always set column explicitly after.
            self.cursor_col = 0;
        }

        // Horizontal movement.
        if col != self.cursor_col {
            w.queue(MoveToColumn(col as u16))?;
        }

        self.cursor_row = row;
        self.cursor_col = col;
        Ok(())
    }
}

/// Splits content into physical terminal lines based on width.
///
/// Always returns at least one (possibly empty) line.
pub fn layout_lines(content: &str, width: usize) -> Vec<Vec<char>> {
    let width = width.max(1);
    let chars: Vec<char> = content.chars().collect();
    if chars.is_empty() {
        return vec![Vec::new()];
    }
    chars.chunks(width).map(|chunk| chunk.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures output bytes so we can inspect what escape sequences the
    /// screen emitted without needing a real terminal.
    fn capture_update(
        screen: &mut Screen,
        desired: &[Vec<char>],
        cursor: (usize, usize),
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        screen.update(&mut buf, desired, cursor).expect("update should succeed");
        buf
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    // --- layout tests ---

    #[test]
    fn layout_empty_produces_one_empty_line() {
        assert_eq!(layout_lines("", 80), vec![Vec::<char>::new()]);
    }

    #[test]
    fn layout_short_produces_one_line() {
        assert_eq!(layout_lines("abc", 80), vec![vec!['a', 'b', 'c']]);
    }

    #[test]
    fn layout_wraps_at_width() {
        let result = layout_lines("abcde", 3);
        assert_eq!(result, vec![vec!['a', 'b', 'c'], vec!['d', 'e']]);
    }

    #[test]
    fn layout_exact_width_is_one_line() {
        assert_eq!(layout_lines("abc", 3), vec![vec!['a', 'b', 'c']]);
    }

    // --- diff tests ---

    #[test]
    fn first_render_emits_full_content() {
        let mut screen = Screen::new(80);
        let desired = vec![chars("> hello")];
        let output = capture_update(&mut screen, &desired, (0, 7));
        let text = String::from_utf8_lossy(&output);
        // Should contain the full prompt text.
        assert!(text.contains("> hello"), "output: {text:?}");
    }

    #[test]
    fn appending_one_char_emits_only_that_char() {
        let mut screen = Screen::new(80);
        let desired_1 = vec![chars("> hell")];
        capture_update(&mut screen, &desired_1, (0, 6));

        let desired_2 = vec![chars("> hello")];
        let output = capture_update(&mut screen, &desired_2, (0, 7));
        let text = String::from_utf8_lossy(&output);
        // Should emit just "o", not the full line.
        assert!(text.contains('o'), "output: {text:?}");
        assert!(!text.contains("> hell"), "should not reprint prefix, output: {text:?}");
    }

    #[test]
    fn identical_content_emits_nothing_except_cursor() {
        let mut screen = Screen::new(80);
        let desired = vec![chars("> hello")];
        capture_update(&mut screen, &desired, (0, 7));

        // Same content, different cursor position.
        let output = capture_update(&mut screen, &desired, (0, 2));
        let text = String::from_utf8_lossy(&output);
        // Should NOT contain the text content.
        assert!(!text.contains("hello"), "output: {text:?}");
    }

    #[test]
    fn shrinking_line_clears_remainder() {
        let mut screen = Screen::new(80);
        let desired_1 = vec![chars("> hello world")];
        capture_update(&mut screen, &desired_1, (0, 13));

        let desired_2 = vec![chars("> hi")];
        let output = capture_update(&mut screen, &desired_2, (0, 4));
        let text = String::from_utf8_lossy(&output);
        // Should contain the clear-to-end-of-line escape.
        // crossterm's UntilNewLine emits \x1b[K
        assert!(text.contains("\x1b[K"), "should clear line tail, output: {text:?}");
    }

    #[test]
    fn wrapping_to_second_line_works() {
        let mut screen = Screen::new(10);
        // "> " is 2 chars, "abcdefghij" is 10 chars = 12 total, wraps at col 10.
        let desired = vec![chars("> abcdefgh"), chars("ij")];
        let output = capture_update(&mut screen, &desired, (1, 2));
        let text = String::from_utf8_lossy(&output);
        // Should contain both line segments.
        assert!(text.contains("> abcdefgh"), "output: {text:?}");
        assert!(text.contains("ij"), "output: {text:?}");
    }

    #[test]
    fn removing_wrapped_line_clears_it() {
        let mut screen = Screen::new(10);
        let desired_1 = vec![chars("> abcdefgh"), chars("ij")];
        capture_update(&mut screen, &desired_1, (1, 2));

        // Now content fits on one line.
        let desired_2 = vec![chars("> ab")];
        let output = capture_update(&mut screen, &desired_2, (0, 4));
        let text = String::from_utf8_lossy(&output);
        // Should contain clear-from-cursor-down (\x1b[J) to wipe the
        // stale second line.
        assert!(text.contains("\x1b[J"), "should clear below, output: {text:?}");
    }
}
