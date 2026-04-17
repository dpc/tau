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
//! - We always use relative cursor movement (MoveUp, MoveDown,
//!   MoveToColumn) — never absolute positioning.

use std::io::{self, Stdout, Write};

use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
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
        stdout: &mut Stdout,
        desired_lines: &[Vec<char>],
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        // Handle empty desired (shouldn't normally happen, but be safe).
        if desired_lines.is_empty() {
            if !self.lines.is_empty() {
                self.move_to(stdout, 0, 0)?;
                stdout.queue(terminal::Clear(ClearType::FromCursorDown))?;
            }
            self.lines.clear();
            self.cursor_row = 0;
            self.cursor_col = 0;
            stdout.flush()?;
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
            self.move_to(stdout, row, common_prefix)?;

            // Print the new content from the first difference onward.
            if common_prefix < desired_slice.len() {
                let new_content: String = desired_slice[common_prefix..].iter().collect();
                stdout.queue(Print(&new_content))?;
                self.cursor_col = desired_slice.len();
            }

            // Clear trailing characters / lines below as needed.
            if has_extra_actual_below {
                // Last desired row and there are stale actual rows below:
                // clear from here to end of screen.
                stdout.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_longer {
                // This row shrank: clear to end of this line only.
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
        }

        // Position the cursor where it should be.
        self.move_to(stdout, desired_cursor.0, desired_cursor.1)?;

        stdout.flush()?;

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
    pub fn erase_all(&mut self, stdout: &mut Stdout) -> io::Result<()> {
        if self.cursor_row > 0 {
            stdout.queue(MoveUp(self.cursor_row as u16))?;
        }
        stdout
            .queue(MoveToColumn(0))?
            .queue(terminal::Clear(ClearType::FromCursorDown))?;
        self.cursor_row = 0;
        self.cursor_col = 0;
        Ok(())
    }

    /// Moves the terminal cursor from the current position to `(row, col)`
    /// using relative movement.
    fn move_to(&mut self, stdout: &mut Stdout, row: usize, col: usize) -> io::Result<()> {
        // Vertical movement.
        if row < self.cursor_row {
            stdout.queue(MoveUp((self.cursor_row - row) as u16))?;
        } else if row > self.cursor_row {
            stdout.queue(MoveDown((row - self.cursor_row) as u16))?;
        }

        // Horizontal movement.
        if col != self.cursor_col || row != self.cursor_row {
            stdout.queue(MoveToColumn(col as u16))?;
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
}
