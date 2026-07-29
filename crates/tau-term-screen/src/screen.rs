//! Screen state tracker and renderer.
//!
//! [`Screen`] maintains an "actual" buffer representing what is
//! currently on the terminal. Two rendering methods use it:
//!
//! - [`Screen::update()`] — **Path 1** (differential update): diffs the visible
//!   viewport against the actual buffer and queues only the escape sequences
//!   needed to update changed cells.
//! - [`Screen::render_scrolling()`] — **Path 2** (scrolling render): diffs the
//!   caller's complete coordinate window, queues changed lines in order, and
//!   lets `\r\n` at the bottom edge push content into terminal scrollback. The
//!   window may be the full logical content or a suffix rebased at the prior
//!   viewport.
//!
//! See `README.md` for the full rendering strategy.
//!
//! Diff approach borrowed from fish shell's `screen.rs`:
//! <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//!
//! Key design choices:
//! - Simple line model (`Vec<Vec<Cell>>`) — no soft-wrap tracking.
//! - Relative cursor movement only (`MoveUp`, `\r`, `\n`, `MoveToColumn`).
//! - `\n` for downward movement (scrolls at bottom edge, unlike `MoveDown`).

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::{Attribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, ClearType};

use crate::style::{
    Align, Cell, Style, StyledBlock, StyledText, is_line_break_grapheme, push_grapheme_cells,
    sanitize_hyperlink_target, visit_styled_graphemes,
};

fn normalize_cell_lines(lines: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    lines
        .iter()
        .map(|line| line.iter().map(Cell::normalized).collect())
        .collect()
}

/// Terminal-column width of a cell slice (sum of individual display widths).
///
/// This is not the same as `cells.len()`: a wide grapheme may occupy multiple
/// terminal columns while zero-width continuations occupy none.
fn cols(cells: &[Cell]) -> usize {
    cells.iter().map(|c| c.col_width()).sum()
}
fn repaint_prefix_for_cluster_boundary(
    mut common_prefix: usize,
    actual: &[Cell],
    desired: &[Cell],
) -> usize {
    if common_prefix == actual.len() && common_prefix == desired.len() {
        return common_prefix;
    }

    while 0 < common_prefix {
        let next_is_continuation = actual
            .get(common_prefix)
            .is_some_and(|cell| cell.col_width() == 0)
            || desired
                .get(common_prefix)
                .is_some_and(|cell| cell.col_width() == 0);
        let prev_is_continuation = actual
            .get(common_prefix - 1)
            .is_some_and(|cell| cell.col_width() == 0)
            || desired
                .get(common_prefix - 1)
                .is_some_and(|cell| cell.col_width() == 0);
        if !next_is_continuation && !prev_is_continuation {
            break;
        }
        common_prefix -= 1;
    }

    common_prefix
}

/// Virtual screen state with diff-based updates.
pub struct Screen {
    /// What we believe is currently displayed on the terminal.
    lines: Vec<Vec<Cell>>,
    /// Current terminal cursor row (relative to prompt start).
    cursor_row: usize,
    /// Current terminal cursor column.
    cursor_col: usize,
    /// Terminal width in columns.
    width: usize,
}

struct ChangedLineRange {
    /// First changed line, as an absolute index into the full scrolling
    /// content.
    first_line: usize,
    /// Last changed line, inclusive. This may be past `all_lines.len()` when
    /// old on-screen rows disappeared and must be cleared.
    last_line: usize,
}

impl Screen {
    /// Creates an empty virtual screen for a terminal of the given width.
    ///
    /// `width` is measured in terminal columns. Passing `0` is treated as a
    /// one-column terminal so layout and cursor math always have a positive
    /// width.
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

    /// Returns the current cursor row (relative to prompt start).
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Diffs the desired content against the actual screen state and queues
    /// only the escape sequences needed to make the terminal match.
    ///
    /// `desired_lines` is the content split into physical rows.
    /// `desired_cursor` is `(row, col)` where the cursor should end up.
    /// The caller owns flushing so it can batch a whole render frame.
    pub fn update(
        &mut self,
        w: &mut impl Write,
        desired_lines: &[Vec<Cell>],
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        let normalized_desired_lines = normalize_cell_lines(desired_lines);
        let desired_lines = normalized_desired_lines.as_slice();

        // Handle empty desired.
        if desired_lines.is_empty() {
            if !self.lines.is_empty() {
                self.move_to(w, 0, 0)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            }
            self.lines.clear();
            self.cursor_row = 0;
            self.cursor_col = 0;
            return Ok(());
        }

        let desired_count = desired_lines.len();

        for (row, desired_line) in desired_lines.iter().enumerate() {
            let actual_line = self.lines.get(row);
            let actual_slice = actual_line.map(|l| l.as_slice()).unwrap_or(&[]);
            let desired_slice = desired_line.as_slice();

            // Find the first cell index where actual and desired differ.
            let common_prefix = actual_slice
                .iter()
                .zip(desired_slice.iter())
                .take_while(|(a, d)| a == d)
                .count();
            let common_prefix =
                repaint_prefix_for_cluster_boundary(common_prefix, actual_slice, desired_slice);

            let is_last_desired = row == desired_count - 1;
            let actual_wider = cols(actual_slice) > cols(desired_slice);
            let has_extra_actual_below = is_last_desired && self.lines.len() > desired_count;

            // Skip if this line is completely unchanged and we don't need
            // to clear below.
            if common_prefix == actual_slice.len()
                && common_prefix == desired_slice.len()
                && !has_extra_actual_below
            {
                continue;
            }

            // Compute column offset for the first changed cell.
            // Done here (before move_to) to avoid borrowing self.lines
            // across the mutable self.move_to call.
            let prefix_cols = cols(&desired_slice[..common_prefix]);

            // Move to the first changed column on this row.
            self.move_to(w, row, prefix_cols)?;

            // Print the new content from the first difference onward.
            if common_prefix < desired_slice.len() {
                emit_styled_cells(w, &desired_slice[common_prefix..])?;
                // Track cursor column as the total column width of the
                // line (not cell count). At exactly `width` columns,
                // the terminal enters "pending wrap" state.
                self.cursor_col = cols(desired_slice);
            }

            // Clear trailing characters / lines below as needed.
            if has_extra_actual_below {
                self.leave_pending_wrap_for_clear(w)?;
                w.queue(terminal::Clear(ClearType::FromCursorDown))?;
            } else if actual_wider {
                w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
        }

        // Position the cursor where it should be.
        self.move_to(w, desired_cursor.0, desired_cursor.1)?;

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

    /// Renders all lines with scrolling support (Pi-style).
    ///
    /// Unlike `update()` which diffs only the new visible viewport, this method
    /// diffs a complete caller-chosen coordinate window against the previously
    /// visible rows and queues changed lines in order. When rendering goes past
    /// the bottom of the terminal, `\r\n` naturally pushes the top row into the
    /// terminal's scrollback buffer.
    ///
    /// Call this instead of `update()` when `viewport_top`
    /// increased (content overflowed the viewport). The caller owns flushing
    /// so it can batch a whole render frame.
    ///
    /// `all_lines` must be complete from the coordinate origin chosen by the
    /// caller through the new content end; it cannot be only the new visible
    /// slice. The caller may use the full logical content with absolute
    /// coordinates, or a suffix beginning at the previous viewport with that
    /// viewport rebased to zero. `prev_viewport_top` locates the old visible
    /// rows in the supplied coordinate window. `height` is terminal height.
    /// `desired_cursor` uses the same coordinate system.
    ///
    /// Inspired by the Pi coding agent's TUI renderer.
    pub fn render_scrolling(
        &mut self,
        w: &mut impl Write,
        all_lines: &[Vec<Cell>],
        prev_viewport_top: usize,
        height: usize,
        desired_cursor: (usize, usize),
    ) -> io::Result<()> {
        let normalized_all_lines = normalize_cell_lines(all_lines);
        let all_lines = normalized_all_lines.as_slice();
        let total = all_lines.len();
        let new_viewport_top = total.saturating_sub(height);

        let Some(changed_range) = self.scrolling_changed_range(all_lines, prev_viewport_top) else {
            let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
            self.move_to(w, cursor_screen, desired_cursor.1)?;
            return Ok(());
        };

        // Clamp first to the previous viewport — we can't render
        // above it (those rows aren't on the physical terminal).
        let render_start = changed_range.first_line.max(prev_viewport_top);
        let mut viewport_top = prev_viewport_top;
        self.scroll_to_render_start(w, render_start, &mut viewport_top, height)?;
        self.render_changed_scrolling_lines(
            w,
            all_lines,
            render_start,
            changed_range.last_line,
            &mut viewport_top,
            height,
        )?;

        // Clear any leftover lines below if content shrunk.
        let old_end = prev_viewport_top + self.lines.len();
        self.clear_shrunk_scrolling_lines(
            w,
            changed_range.last_line + 1,
            old_end,
            viewport_top,
            height,
        )?;

        // Position cursor.
        let cursor_screen = desired_cursor.0.saturating_sub(new_viewport_top);
        self.move_to(w, cursor_screen, desired_cursor.1)?;
        // Update tracked state to the new visible viewport.
        self.lines = all_lines[new_viewport_top..].to_vec();
        self.cursor_row = cursor_screen;
        self.cursor_col = desired_cursor.1;

        Ok(())
    }

    fn scrolling_changed_range(
        &self,
        all_lines: &[Vec<Cell>],
        prev_viewport_top: usize,
    ) -> Option<ChangedLineRange> {
        // Find first and last changed line across the part of the content that
        // is, or was, physically represented on the terminal. Lines above the
        // previous viewport are already in scrollback; treating them as changed
        // would force us to rewrite the top visible rows just before they drop
        // into scrollback.
        //
        // Keep missing lines distinct from present-but-empty lines: appending
        // an empty physical row still needs to scroll the viewport.
        let max_idx = all_lines.len().max(prev_viewport_top + self.lines.len());
        let mut changed_range: Option<ChangedLineRange> = None;
        for line_idx in prev_viewport_top..max_idx {
            let old = self
                .lines
                .get(line_idx - prev_viewport_top)
                .map(|line| line.as_slice());
            let new = all_lines.get(line_idx).map(|line| line.as_slice());
            if old != new {
                changed_range = Some(match changed_range {
                    Some(range) => ChangedLineRange {
                        first_line: range.first_line,
                        last_line: line_idx,
                    },
                    None => ChangedLineRange {
                        first_line: line_idx,
                        last_line: line_idx,
                    },
                });
            }
        }
        changed_range
    }

    fn scroll_to_render_start(
        &mut self,
        w: &mut impl Write,
        render_start: usize,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        // Mutates both the tracked viewport and cursor row via natural terminal
        // scrolling before addressing `render_start` within the new viewport.
        let viewport_bottom = *viewport_top + height - 1;
        if render_start > viewport_bottom {
            let to_bottom = (height - 1).saturating_sub(self.cursor_row);
            self.move_down_rows(w, to_bottom)?;
            let scroll = render_start - viewport_bottom;
            self.move_down_rows(w, scroll)?;
            *viewport_top += scroll;
            self.cursor_row = height - 1;
        }
        let start_screen_row = render_start - *viewport_top;
        self.move_to(w, start_screen_row, 0)
    }

    fn render_changed_scrolling_lines(
        &mut self,
        w: &mut impl Write,
        all_lines: &[Vec<Cell>],
        render_start: usize,
        render_last_line: usize,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        // `render_last_line` is inclusive. Missing rows are deliberate: they
        // represent old content that disappeared and should be cleared.
        for line_idx in render_start..=render_last_line {
            if render_start < line_idx {
                self.advance_scrolling_render_row(w, viewport_top, height)?;
            }
            w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            if let Some(line) = all_lines.get(line_idx) {
                emit_styled_cells(w, line)?;
            }
            self.cursor_col = all_lines.get(line_idx).map(|line| cols(line)).unwrap_or(0);
        }
        Ok(())
    }

    fn clear_shrunk_scrolling_lines(
        &mut self,
        w: &mut impl Write,
        rendered_up_to: usize,
        old_end: usize,
        viewport_top: usize,
        height: usize,
    ) -> io::Result<()> {
        // Called immediately after rendering through `rendered_up_to - 1`; any
        // remaining old rows still inside the current viewport must be cleared.
        if old_end <= rendered_up_to {
            return Ok(());
        }
        for _ in rendered_up_to..old_end.min(viewport_top + height) {
            self.move_down_one(w)?;
            w.queue(terminal::Clear(ClearType::UntilNewLine))?;
            if self.cursor_row + 1 < height {
                self.cursor_row += 1;
            }
        }
        Ok(())
    }

    fn advance_scrolling_render_row(
        &mut self,
        w: &mut impl Write,
        viewport_top: &mut usize,
        height: usize,
    ) -> io::Result<()> {
        self.move_down_one(w)?;
        let screen_row = self.cursor_row + 1;
        if screen_row >= height {
            // Moving down scrolled the terminal.
            *viewport_top += 1;
            self.cursor_row = height - 1;
        } else {
            self.cursor_row = screen_row;
        }
        Ok(())
    }

    fn move_down_rows(&mut self, w: &mut impl Write, rows: usize) -> io::Result<()> {
        for _ in 0..rows {
            self.move_down_one(w)?;
        }
        Ok(())
    }

    /// Number of physical lines currently tracked as on-screen.
    pub fn actual_line_count(&self) -> usize {
        self.lines.len()
    }

    /// Overwrites the internal state to match what is currently on the
    /// terminal. Call after a full render to prepare for future
    /// differential updates.
    pub fn reset_to(&mut self, lines: Vec<Vec<Cell>>, cursor_row: usize, cursor_col: usize) {
        self.lines = normalize_cell_lines(&lines);
        self.cursor_row = cursor_row;
        self.cursor_col = cursor_col;
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
            // Use an explicit column reset before LF for downward movement:
            // - \n scrolls at the screen bottom (unlike MoveDown which silently stops)
            // - the column reset is needed because \n alone preserves the column, and
            //   pending-wrap state after an exact-width line is unsafe
            let down = row - self.cursor_row;
            for _ in 0..down {
                self.move_down_one(w)?;
            }
        }

        // Horizontal movement.
        if col != self.cursor_col {
            w.queue(MoveToColumn(col as u16))?;
        }

        self.cursor_row = row;
        self.cursor_col = col;
        Ok(())
    }

    fn leave_pending_wrap_for_clear(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.width <= self.cursor_col {
            self.move_down_one(w)?;
            self.cursor_row += 1;
        }
        Ok(())
    }

    /// Moves down one terminal row after first forcing the terminal
    /// cursor to column zero.
    ///
    /// This avoids relying on CR/LF behavior while the terminal is in
    /// pending-wrap state after printing in the last column.
    fn move_down_one(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.cursor_col != 0 {
            w.queue(MoveToColumn(0))?;
            self.cursor_col = 0;
        }
        w.queue(Print("\n"))?;
        Ok(())
    }
}

/// Emits a sequence of styled cells to the writer.
///
/// Cell constructors sanitize terminal controls, but this function also applies
/// the same final output policy so hand-built public `Cell` values cannot emit
/// raw control bytes.
///
/// Tracks style changes and only emits escape codes when the style
/// differs from the previous cell. Resets to default style at the end
/// if any non-default style was active.
///
/// The caller must ensure the terminal is in default style state before
/// calling this function.
pub fn emit_styled_cells(w: &mut impl Write, cells: &[Cell]) -> io::Result<()> {
    let mut current = Style::default();
    let mut current_hyperlink: Option<&str> = None;

    for cell in cells {
        let hyperlink = cell
            .hyperlink
            .as_deref()
            .and_then(sanitize_hyperlink_target);
        if hyperlink != current_hyperlink {
            if current_hyperlink.is_some() {
                w.write_all(b"\x1b]8;;\x1b\\")?;
            }
            if let Some(target) = hyperlink {
                write_osc8_open(w, target)?;
            }
            current_hyperlink = hyperlink;
        }
        if cell.style != current {
            // Reset to clean slate, then apply new style.
            if current != Style::default() {
                w.queue(SetAttribute(Attribute::Reset))?;
            }
            if cell.style != Style::default() {
                apply_style(w, &cell.style)?;
            }
            current = cell.style;
        }
        w.queue(Print(cell.normalized().ch))?;
    }

    if current_hyperlink.is_some() {
        w.write_all(b"\x1b]8;;\x1b\\")?;
    }
    // Restore default state.
    if current != Style::default() {
        w.queue(SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

fn write_osc8_open(w: &mut impl Write, target: &str) -> io::Result<()> {
    if let Some(target) = sanitize_hyperlink_target(target) {
        write!(w, "\x1b]8;;{target}\x1b\\")?;
    }
    Ok(())
}

/// Writes one safely bounded OSC 8 hyperlink.
///
/// Unsafe targets are rendered as plain labels so callers cannot inject
/// terminal controls through either the target or label.
pub fn write_osc8_hyperlink(w: &mut impl Write, label: &str, target: &str) -> io::Result<()> {
    let safe_label: String = label
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect();
    if sanitize_hyperlink_target(target).is_none() {
        return w.write_all(safe_label.as_bytes());
    }
    write_osc8_open(w, target)?;
    w.write_all(safe_label.as_bytes())?;
    w.write_all(b"\x1b]8;;\x1b\\")
}

/// Applies non-default style attributes (without resetting first).
fn apply_style(w: &mut impl Write, style: &Style) -> io::Result<()> {
    if let Some(fg) = style.fg {
        w.queue(SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        w.queue(SetBackgroundColor(bg))?;
    }
    if style.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if style.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }
    if style.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    if style.strikethrough {
        w.queue(SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

/// Splits styled content into physical terminal lines based on width.
///
/// Handles newlines within spans (each newline starts a new logical
/// line) and wraps at the terminal width. Always returns at least one
/// (possibly empty) line.
///
/// By default a single trailing empty logical line — the one
/// introduced by a `\n` at the very end of the input — is collapsed
/// away. That's what static blocks (agent responses, tool output,
/// anything ending with the LLM/shell's terminating `\n`) want so
/// they don't render a phantom blank row at the bottom. Pass
/// `preserve_last_newline(true)` for the live input buffer, where a
/// buffer ending in `\n` (after Shift+Enter / Alt+Enter) needs the
/// extra row for the cursor to sit on.
#[bon::builder]
pub fn layout_lines(
    content: &StyledText,
    width: usize,
    #[builder(default = false)] preserve_last_newline: bool,
) -> Vec<Vec<Cell>> {
    let width = width.max(1);

    // Split into logical lines at newlines.
    let mut logical_lines: Vec<Vec<Cell>> = vec![Vec::new()];
    visit_styled_graphemes(content.spans(), |grapheme, style, hyperlink| {
        if is_line_break_grapheme(grapheme) {
            logical_lines.push(Vec::new());
        } else {
            let line = logical_lines
                .last_mut()
                .expect("logical_lines always has at least one entry");
            push_grapheme_cells(line, grapheme, style, hyperlink);
        }
    });

    if !preserve_last_newline
        && logical_lines.len() > 1
        && logical_lines.last().is_some_and(|l| l.is_empty())
    {
        logical_lines.pop();
    }

    // Wrap each logical line at width (measured in terminal columns,
    // not cell count — wide chars like emoji occupy 2 columns).
    let mut result: Vec<Vec<Cell>> = Vec::new();
    // ast-grep-ignore: filter-in-loop
    for line in logical_lines {
        if line.is_empty() {
            result.push(Vec::new());
        } else {
            let mut row = Vec::new();
            let mut skip_zero_width_suffix = false;
            let mut col = 0usize;
            for cell in line {
                let w = cell.col_width();
                if skip_zero_width_suffix && w == 0 {
                    continue;
                }
                skip_zero_width_suffix = false;
                if width < w {
                    if !row.is_empty() {
                        result.push(row);
                        row = Vec::new();
                    }
                    row.push(Cell::new('�', cell.style).with_hyperlink(cell.hyperlink));
                    result.push(row);
                    row = Vec::new();
                    col = 0;
                    skip_zero_width_suffix = true;
                    continue;
                }
                if width < col + w && !row.is_empty() {
                    result.push(row);
                    row = Vec::new();
                    col = 0;
                }
                row.push(cell);
                col += w;
            }
            if !row.is_empty() {
                result.push(row);
            }
        }
    }

    if result.is_empty() {
        result.push(Vec::new());
    }

    result
}

/// Lays out a [`StyledBlock`] into physical terminal lines.
///
/// Subtracts margins from `width`, wraps content to the remaining
/// space, applies alignment, and fills background. Each returned row has total
/// display width of exactly `width` terminal columns. The number of [`Cell`]
/// values is not necessarily `width`, because wide graphemes can occupy
/// multiple columns and continuation cells can occupy none.
pub fn layout_block(block: &StyledBlock, width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let requested_ml = block.margin_left as usize;
    let requested_mr = block.margin_right as usize;
    let ml = requested_ml.min(width.saturating_sub(1));
    let remaining_after_ml = width.saturating_sub(ml);
    let mr = requested_mr.min(remaining_after_ml.saturating_sub(1));
    let content_width = width.saturating_sub(ml + mr).max(1);
    let fill_style = Style {
        bg: block.bg,
        ..Style::default()
    };
    let fill = Cell::new(' ', fill_style);

    let mut content_lines = Vec::new();
    if let Some(priority_line) = &block.priority_line {
        let priority_layout = priority_line.layout_with_fill(content_width, fill.clone());
        content_lines.push(priority_layout.row);
        if priority_layout.required_items_fit && !block.priority_line_body.is_empty() {
            content_lines.extend(
                layout_lines()
                    .content(&block.priority_line_body)
                    .width(content_width)
                    .call(),
            );
        }
    } else {
        content_lines.extend(
            layout_lines()
                .content(&block.content)
                .width(content_width)
                .call(),
        );
    }
    if block.priority_line.is_none()
        && block.align == Align::Left
        && !block.right_content.is_empty()
        && content_lines.len() == 1
    {
        let right_cells = block.right_content.to_cells();
        let left_cols = cols(&content_lines[0]);
        let right_cols = cols(&right_cells);
        if left_cols + 1 + right_cols <= content_width {
            let padding = content_width - left_cols - right_cols;
            content_lines[0].extend(std::iter::repeat_n(Cell::plain(' '), padding));
            content_lines[0].extend(right_cells);
        }
    }

    content_lines
        .iter()
        .map(|line| {
            let mut row = Vec::with_capacity(width);

            // Left margin (always default bg, not block bg).
            row.extend(std::iter::repeat_n(Cell::plain(' '), ml));

            // Content with alignment (column width, not cell count).
            let cw = cols(line);
            let padding = content_width.saturating_sub(cw);
            match block.align {
                Align::Left => {
                    row.extend(line.iter().cloned());
                    row.extend(std::iter::repeat_n(fill.clone(), padding));
                }
                Align::Center => {
                    let left = padding / 2;
                    let right = padding - left;
                    row.extend(std::iter::repeat_n(fill.clone(), left));
                    row.extend(line.iter().cloned());
                    row.extend(std::iter::repeat_n(fill.clone(), right));
                }
            }

            // Right margin (always default bg).
            row.extend(std::iter::repeat_n(Cell::plain(' '), mr));

            // Apply block bg to content cells that don't set their own.
            // Use cell indices (not column count) to avoid
            // overrun when wide chars reduce the cell count.
            if let Some(bg) = block.bg {
                let content_end = row.len().saturating_sub(mr);
                for cell in &mut row[ml..content_end] {
                    if cell.style.bg.is_none() {
                        cell.style.bg = Some(bg);
                    }
                }
            }

            row
        })
        .collect()
}

#[cfg(test)]
mod tests;
