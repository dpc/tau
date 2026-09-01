//! Synchronous, blocking, single-select terminal list picker.
//!
//! Three entry points are exposed:
//!
//! - [`pick`] — full convenience. Enables raw mode and writes to `stderr`. Must
//!   not be called from inside a TUI that already owns raw mode.
//! - [`pick_with_writer`] — enables raw mode but writes the picker frame to a
//!   caller-provided writer.
//! - [`pick_with_io`] — does *not* manage raw mode. Intended for tests or
//!   non-terminal hosts that drive the picker via in-memory streams.
//!
//! The crate's terminal-ownership and input contracts are summarized in
//! `ARCH-tau-cli-picker`.

mod error;
mod item;
mod key;
mod raw_mode;

#[cfg(test)]
mod tests;

use std::io;

use tau_term_screen::screen::Screen;
use tau_term_screen::style::StyledText;
use tau_term_screen::truncate_to_width;

pub use crate::error::PickerError;
pub use crate::item::PickerItem;
use crate::key::{PickerEvent, PickerKey, read_byte_key, read_terminal_event};
use crate::raw_mode::{RawModeCleanup, RawModeGuard};

/// Prompts the user to pick one of `items`, rendering to `stderr`.
///
/// Returns the original index of the selected item. Disabled items are rendered
/// but skipped by navigation and cannot be selected.
///
/// Enables terminal raw mode for the duration of the call. The picker
/// must therefore not be invoked while another component already owns
/// raw mode — the inner [`Drop`] would silently restore cooked mode and
/// strand the parent. Simple byte-stream hosts that manage raw mode themselves
/// can use [`pick_with_io`]. Embedded crossterm/TUI callers should not use this
/// picker as a widget; they need a future host-event API that accepts
/// caller-provided events, resize notifications, and size samples.
///
/// The picker frame is written to `stderr` (fd 2) so that the typical
/// `cli | tool` pipe shape leaves `stdout` untouched.
///
/// # Errors
///
/// Returns [`PickerError::Empty`] when no items are supplied,
/// [`PickerError::NoEnabledItems`] when every item is disabled,
/// [`PickerError::Cancelled`] when the user cancels, and
/// [`PickerError::Io`] for terminal raw-mode, input, rendering, or normal
/// selection cleanup failures. Cleanup after cancellation or input errors is
/// best-effort and does not replace the original error, except that explicit
/// raw-mode restoration failures from this raw-mode-owning entry point are
/// reported as [`PickerError::Io`] even if the picker otherwise selected,
/// cancelled, or hit an input/rendering error, so callers are not told the
/// terminal was safely restored when it may still be raw.
pub fn pick(prompt: &str, items: &[PickerItem]) -> Result<usize, PickerError> {
    pick_with_raw_mode(
        prompt,
        items,
        io::stderr(),
        RawModeGuard::enable,
        read_terminal_event,
        terminal_size,
    )
}

/// Like [`pick`], but writes the picker frame to `writer`.
///
/// Returns the original index of the selected item. Disabled items are rendered
/// but skipped by navigation and cannot be selected.
///
/// Enables terminal raw mode for the duration of the call; the same
/// caveat as [`pick`] applies.
///
/// # Errors
///
/// Returns the same errors as [`pick`], including raw-mode setup failures and
/// writer I/O failures.
pub fn pick_with_writer(
    prompt: &str,
    items: &[PickerItem],
    writer: impl io::Write,
) -> Result<usize, PickerError> {
    pick_with_raw_mode(
        prompt,
        items,
        writer,
        RawModeGuard::enable,
        read_terminal_event,
        terminal_size,
    )
}

/// Drives the picker against caller-provided byte-stream IO.
///
/// Returns the original index of the selected item. Disabled items are rendered
/// but skipped by navigation and cannot be selected.
///
/// Does **not** toggle terminal raw mode. Intended for tests and simple
/// byte-stream hosts. Embedded crossterm/TUI hosts still need a public API that
/// accepts host-provided events, resize notifications, and size samples.
///
/// EOF, bare Escape, Ctrl-C, Ctrl-D, and `q` cancel the picker. The byte-stream
/// reader decodes Enter, Tab, BackTab, `j`/`k`, and simple CSI Up/Down arrow
/// sequences; other printable keys are ignored, and Space is reserved.
///
/// # Errors
///
/// Returns [`PickerError::Empty`] when no items are supplied,
/// [`PickerError::NoEnabledItems`] when every item is disabled,
/// [`PickerError::Cancelled`] when the input stream cancels or reaches EOF, and
/// [`PickerError::Io`] for reader, writer, rendering, or normal selection
/// cleanup failures. Cleanup after cancellation or input errors is best-effort
/// and does not replace the original error.
pub fn pick_with_io(
    prompt: &str,
    items: &[PickerItem],
    writer: impl io::Write,
    mut reader: impl io::Read,
) -> Result<usize, PickerError> {
    pick_with_event_reader(
        prompt,
        items,
        writer,
        || read_byte_key(&mut reader).map(PickerEvent::Key),
        terminal_size,
    )
}

fn pick_with_raw_mode<G: RawModeCleanup>(
    prompt: &str,
    items: &[PickerItem],
    writer: impl io::Write,
    enable_raw_mode: impl FnOnce() -> io::Result<G>,
    read_event: impl FnMut() -> io::Result<PickerEvent>,
    current_size: impl FnMut() -> (usize, usize),
) -> Result<usize, PickerError> {
    validate_items(items)?;
    let mut raw = enable_raw_mode()?;
    let result = pick_with_event_reader(prompt, items, writer, read_event, current_size);
    let restore_result = raw.restore_raw_mode();

    match restore_result {
        Ok(()) => result,
        Err(err) => Err(PickerError::Io(err)),
    }
}

fn validate_items(items: &[PickerItem]) -> Result<usize, PickerError> {
    if items.is_empty() {
        return Err(PickerError::Empty);
    }
    items
        .iter()
        .position(PickerItem::is_enabled)
        .ok_or(PickerError::NoEnabledItems)
}

fn pick_with_event_reader(
    prompt: &str,
    items: &[PickerItem],
    mut writer: impl io::Write,
    mut read_event: impl FnMut() -> io::Result<PickerEvent>,
    mut current_size: impl FnMut() -> (usize, usize),
) -> Result<usize, PickerError> {
    let mut selected = validate_items(items)?;
    let (mut width, mut height) = current_size();
    let mut screen = Screen::new(width);

    let result = (|| -> Result<usize, PickerError> {
        render(&mut screen, &mut writer, prompt, items, selected, height)?;
        loop {
            match read_event()? {
                PickerEvent::Key(PickerKey::Down) => {
                    selected = adjacent_enabled_item(items, selected, NavigationDirection::Forward);
                }
                PickerEvent::Key(PickerKey::Up) => {
                    selected =
                        adjacent_enabled_item(items, selected, NavigationDirection::Backward);
                }
                PickerEvent::Key(PickerKey::Enter) => {
                    clear_picker_frame(&mut screen, &mut writer)?;
                    return Ok(selected);
                }
                PickerEvent::Key(PickerKey::Cancelled) => {
                    return Err(PickerError::Cancelled);
                }
                PickerEvent::Key(PickerKey::Ignored) => {}
                PickerEvent::Resize {
                    width: new_width,
                    height: new_height,
                } => {
                    let new_width = resize_dimension(new_width, width);
                    let new_height = resize_dimension(new_height, height);
                    screen.erase_all(&mut writer)?;
                    screen.invalidate();
                    if new_width != width {
                        screen.set_width(new_width);
                        width = new_width;
                    }
                    height = new_height;
                }
            }
            render(&mut screen, &mut writer, prompt, items, selected, height)?;
        }
    })();

    if result.is_err() {
        let _ = force_clear_picker_frame(&mut screen, &mut writer);
    }
    result
}

fn clear_picker_frame(screen: &mut Screen, writer: &mut impl io::Write) -> io::Result<()> {
    screen.update(writer, &[], (0, 0))?;
    writer.flush()
}

fn force_clear_picker_frame(screen: &mut Screen, writer: &mut impl io::Write) -> io::Result<()> {
    screen.erase_all(writer)?;
    screen.invalidate();
    writer.flush()
}

fn render(
    screen: &mut Screen,
    writer: &mut impl io::Write,
    prompt: &str,
    items: &[PickerItem],
    selected: usize,
    terminal_height: usize,
) -> io::Result<()> {
    let (lines, cursor_row) =
        picker_lines(prompt, items, selected, screen.width(), terminal_height);
    screen.update(writer, &lines, (cursor_row, 0))?;
    writer.flush()
}

fn picker_lines(
    prompt: &str,
    items: &[PickerItem],
    selected: usize,
    width: usize,
    terminal_height: usize,
) -> (Vec<Vec<tau_term_screen::style::Cell>>, usize) {
    if terminal_height <= 1 {
        let item = &items[selected];
        let marker = if item.is_enabled() { '>' } else { 'X' };
        let line = truncate_to_width(&format!("{marker} {} — ? {prompt}", item.label()), width);
        return (vec![StyledText::from(line).to_cells()], 0);
    }

    // Reserve one row for the prompt; leave at least one item visible.
    let visible = terminal_height.saturating_sub(1).max(1);
    let window = visible_window(items.len(), selected, visible);
    let mut lines = Vec::with_capacity(window.len() + 1);
    lines.push(StyledText::from(truncate_to_width(&format!("? {prompt}"), width)).to_cells());
    for (idx, item) in items.iter().enumerate().take(window.end).skip(window.start) {
        let marker = if !item.is_enabled() {
            'X'
        } else if idx == selected {
            '>'
        } else {
            ' '
        };
        let line = truncate_to_width(&format!("{marker} {}", item.label()), width);
        lines.push(StyledText::from(line).to_cells());
    }
    (lines, selected - window.start + 1)
}

/// Returns `[start, end)` of the items to render, ensuring `selected`
/// is within view.
fn visible_window(total: usize, selected: usize, visible: usize) -> std::ops::Range<usize> {
    if total <= visible {
        return 0..total;
    }
    let half = visible / 2;
    let mut start = selected.saturating_sub(half);
    if total < start + visible {
        start = total - visible;
    }
    start..start + visible
}

enum NavigationDirection {
    Forward,
    Backward,
}

fn adjacent_enabled_item(
    items: &[PickerItem],
    selected: usize,
    direction: NavigationDirection,
) -> usize {
    for offset in 1..items.len() {
        let idx = match direction {
            NavigationDirection::Forward => (selected + offset) % items.len(),
            NavigationDirection::Backward => (selected + items.len() - offset) % items.len(),
        };
        if items[idx].is_enabled() {
            return idx;
        }
    }
    selected
}

fn resize_dimension(reported: u16, current: usize) -> usize {
    if 0 < reported {
        usize::from(reported)
    } else {
        current.max(1)
    }
}

const DEFAULT_TERMINAL_WIDTH: usize = 80;
const DEFAULT_TERMINAL_HEIGHT: usize = 24;

fn terminal_size() -> (usize, usize) {
    crossterm::terminal::size().map_or(
        (DEFAULT_TERMINAL_WIDTH, DEFAULT_TERMINAL_HEIGHT),
        |(w, h)| (usize::from(w).max(1), usize::from(h).max(1)),
    )
}
