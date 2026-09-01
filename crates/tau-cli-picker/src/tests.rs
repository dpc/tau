use std::collections::VecDeque;
use std::error as path_std_error;
use std::io::{self, Cursor};
use std::sync::{Arc, Mutex};

use crate::key::{PickerEvent, PickerKey};
use crate::raw_mode::RawModeCleanup;
use crate::{
    NavigationDirection, PickerError, PickerItem, adjacent_enabled_item, pick_with_event_reader,
    pick_with_io, pick_with_raw_mode, picker_lines,
};

struct InterruptedReader {
    interrupted: bool,
    inner: Cursor<Vec<u8>>,
}

impl InterruptedReader {
    fn new(bytes: &[u8]) -> Self {
        Self {
            interrupted: false,
            inner: Cursor::new(bytes.to_vec()),
        }
    }
}

impl io::Read for InterruptedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.inner.read(buf)
    }
}

fn items(labels: &[&str]) -> Vec<PickerItem> {
    labels.iter().map(|l| PickerItem::enabled(*l)).collect()
}

fn run(reader_bytes: &[u8], items: &[PickerItem]) -> Result<usize, PickerError> {
    let writer = Vec::<u8>::new();
    let reader = Cursor::new(reader_bytes.to_vec());
    pick_with_io("pick", items, writer, reader)
}

/// Protects byte-stream hosts from transient EINTR-style read interruptions
/// before a valid selection key is read.
#[test]
fn interrupted_byte_read_is_retried() {
    let it = items(&["one", "two"]);
    let picked = pick_with_io("pick", &it, Vec::<u8>::new(), InterruptedReader::new(b"\n"))
        .expect("interrupted read should be retried before enter");

    assert_eq!(picked, 0);
}

/// Verifies Enter accepts the initial enabled item, protecting the primary
/// selection path used by byte-stream hosts.
#[test]
fn enter_selects_first_enabled() {
    let it = items(&["one", "two"]);
    assert_eq!(run(b"\n", &it).expect("enter picks 0"), 0);
}

/// Verifies carriage return is treated like Enter so hosts using CR line
/// endings can still accept the highlighted item.
#[test]
fn cr_also_selects() {
    let it = items(&["one"]);
    assert_eq!(run(b"\r", &it).expect("cr picks 0"), 0);
}

/// Ensures Space remains ignored rather than selecting, preserving room for a
/// future multi-select mode without changing current behavior.
#[test]
fn space_does_not_select() {
    // Space must NOT be Enter — reserved for a possible multi-select.
    // After space the buffer ends (EOF), which the byte reader treats
    // as Cancelled, so the call should not return Ok.
    let it = items(&["one", "two"]);
    assert!(matches!(run(b" ", &it), Err(PickerError::Cancelled)));
}

/// Verifies vim-style j/k navigation so keyboard-only users can move through
/// choices without arrow keys.
#[test]
fn j_moves_down_k_moves_up() {
    let it = items(&["a", "b", "c"]);
    assert_eq!(run(b"jj\n", &it).expect("jj enter"), 2);
    assert_eq!(run(b"jjk\n", &it).expect("jjk enter"), 1);
}

/// Verifies byte-stream CSI arrow decoding moves selection in both directions,
/// protecting common terminal navigation input.
#[test]
fn arrow_keys_move() {
    let it = items(&["a", "b", "c"]);
    // Down arrow = ESC [ B
    assert_eq!(run(b"\x1b[B\x1b[B\n", &it).expect("two downs"), 2);
    // Up arrow from index 2
    assert_eq!(
        run(b"\x1b[B\x1b[B\x1b[A\n", &it).expect("two downs one up"),
        1
    );
}

/// Verifies Tab and BackTab mirror down/up navigation for keyboard-only flows
/// and shell-style completion muscle memory.
#[test]
fn tab_moves_down_backtab_moves_up() {
    let it = items(&["a", "b", "c"]);
    assert_eq!(run(b"\t\t\n", &it).expect("two tabs"), 2);
    // BackTab = ESC [ Z
    assert_eq!(run(b"\t\t\x1b[Z\n", &it).expect("two tabs one backtab"), 1);
}

/// Ensures Ctrl-C cancels instead of selecting, matching terminal interrupt
/// expectations for interactive prompts.
#[test]
fn ctrl_c_cancels() {
    let it = items(&["a", "b"]);
    assert!(matches!(run(b"\x03", &it), Err(PickerError::Cancelled)));
}

/// Ensures Ctrl-D cancels like EOF so closed input streams do not select an
/// item accidentally.
#[test]
fn ctrl_d_cancels() {
    // Ctrl-D commonly signals EOF; byte-stream callers should get the same
    // cancellation result as terminal users pressing Ctrl-C or Escape.
    let it = items(&["a", "b"]);
    assert!(matches!(run(b"\x04", &it), Err(PickerError::Cancelled)));
}

/// Ensures q cancels the picker, preserving the documented keyboard shortcut
/// for aborting selection.
#[test]
fn q_cancels() {
    let it = items(&["a", "b"]);
    assert!(matches!(run(b"q", &it), Err(PickerError::Cancelled)));
}

/// Ensures bare Escape cancels promptly in buffered byte-stream tests rather
/// than being interpreted as an incomplete control sequence.
#[test]
fn bare_esc_cancels() {
    // ESC followed by EOF must cancel — not block, not eat phantom bytes.
    let it = items(&["a", "b"]);
    assert!(matches!(run(b"\x1b", &it), Err(PickerError::Cancelled)));
}

/// Ensures Escape followed by a non-CSI byte is treated as cancellation,
/// preserving the byte-reader ambiguity contract.
#[test]
fn esc_then_unrelated_byte_cancels() {
    let it = items(&["a", "b"]);
    // ESC followed by a non-`[` byte is treated as bare ESC.
    assert!(matches!(run(b"\x1bx", &it), Err(PickerError::Cancelled)));
}

/// Verifies empty item lists return the documented validation error instead of
/// attempting to render or read input.
#[test]
fn empty_items_errors() {
    let it: Vec<PickerItem> = Vec::new();
    assert!(matches!(run(b"\n", &it), Err(PickerError::Empty)));
}

/// Verifies all-disabled item lists return the documented validation error so
/// callers know no selection is possible.
#[test]
fn all_disabled_errors() {
    let it = vec![PickerItem::disabled("a"), PickerItem::disabled("b")];
    assert!(matches!(run(b"\n", &it), Err(PickerError::NoEnabledItems)));
}

/// Ensures generic error reporters can recover the underlying I/O cause from
/// picker I/O failures instead of losing source-chain context.
#[test]
fn io_error_exposes_source() {
    let err = PickerError::Io(io::Error::other("synthetic io error"));
    let source = path_std_error::Error::source(&err).expect("io source should be exposed");

    assert_eq!(source.to_string(), "synthetic io error");
}

/// Ensures public raw-mode wrappers can validate invalid item lists before
/// touching terminal state, preserving deterministic validation errors.
#[test]
fn raw_mode_picker_validates_items_before_enabling_raw_mode() {
    let mut raw_enabled = false;
    let result = pick_with_raw_mode(
        "pick",
        &[],
        Vec::<u8>::new(),
        || {
            raw_enabled = true;
            Ok(FailingRawModeGuard {
                restore_error: None,
            })
        },
        || panic!("invalid items should not read input"),
        || panic!("invalid items should not sample terminal size"),
    );

    assert!(matches!(result, Err(PickerError::Empty)));
    assert!(!raw_enabled, "raw mode should not be enabled");

    let mut raw_enabled = false;
    let disabled = vec![PickerItem::disabled("a")];
    let result = pick_with_raw_mode(
        "pick",
        &disabled,
        Vec::<u8>::new(),
        || {
            raw_enabled = true;
            Ok(FailingRawModeGuard {
                restore_error: None,
            })
        },
        || panic!("invalid items should not read input"),
        || panic!("invalid items should not sample terminal size"),
    );

    assert!(matches!(result, Err(PickerError::NoEnabledItems)));
    assert!(!raw_enabled, "raw mode should not be enabled");
}

/// Ensures a raw-mode setup failure reaches callers unchanged and prevents the
/// picker from sampling, rendering, or reading while terminal ownership is
/// absent.
#[test]
fn raw_mode_setup_failure_short_circuits_picker() {
    let it = items(&["one"]);
    let err = pick_with_raw_mode(
        "pick",
        &it,
        Vec::<u8>::new(),
        || Err::<FailingRawModeGuard, _>(io::Error::other("synthetic raw setup error")),
        || panic!("raw-mode setup failure should not read input"),
        || panic!("raw-mode setup failure should not sample terminal size"),
    )
    .expect_err("raw-mode setup failure should propagate");

    match err {
        PickerError::Io(source) => assert_eq!(source.to_string(), "synthetic raw setup error"),
        other => panic!("expected raw setup IO error, got {other:?}"),
    }
}

/// Ensures disabled rows stay visible but are skipped by navigation and cannot
/// become the selected result.
#[test]
fn disabled_items_are_skipped() {
    let it = vec![
        PickerItem::enabled("a"),
        PickerItem::disabled("b"),
        PickerItem::enabled("c"),
    ];
    // First enabled is index 0; one j should skip the disabled to land on 2.
    assert_eq!(run(b"j\n", &it).expect("skip disabled"), 2);
    // Two js wraps back to 0.
    assert_eq!(run(b"jj\n", &it).expect("skip disabled twice"), 0);
}

/// Proves named traversal directions preserve cyclic wrapping, disabled-row
/// skipping, and the current selection when it is the only enabled row.
#[test]
fn adjacent_enabled_item_preserves_navigation_direction_behavior() {
    let with_disabled_middle = vec![
        PickerItem::enabled("a"),
        PickerItem::disabled("b"),
        PickerItem::enabled("c"),
    ];

    assert_eq!(
        adjacent_enabled_item(&with_disabled_middle, 0, NavigationDirection::Forward),
        2,
        "forward skips a disabled row",
    );
    assert_eq!(
        adjacent_enabled_item(&with_disabled_middle, 2, NavigationDirection::Backward),
        0,
        "backward skips a disabled row",
    );
    assert_eq!(
        adjacent_enabled_item(&with_disabled_middle, 2, NavigationDirection::Forward),
        0,
        "forward wraps",
    );
    assert_eq!(
        adjacent_enabled_item(&with_disabled_middle, 0, NavigationDirection::Backward),
        2,
        "backward wraps",
    );

    let only_enabled = vec![
        PickerItem::disabled("a"),
        PickerItem::enabled("b"),
        PickerItem::disabled("c"),
    ];
    assert_eq!(
        adjacent_enabled_item(&only_enabled, 1, NavigationDirection::Forward),
        1,
        "forward retains the sole enabled selection",
    );
    assert_eq!(
        adjacent_enabled_item(&only_enabled, 1, NavigationDirection::Backward),
        1,
        "backward retains the sole enabled selection",
    );
}

/// Verifies the initial cursor lands on the first enabled item when earlier
/// rows are disabled.
#[test]
fn first_enabled_is_initial_selection() {
    let it = vec![
        PickerItem::disabled("a"),
        PickerItem::disabled("b"),
        PickerItem::enabled("c"),
    ];
    assert_eq!(run(b"\n", &it).expect("third is enabled"), 2);
}

/// Ensures unrelated printable keys are ignored so accidental typing does not
/// move, select, or cancel the picker.
#[test]
fn byte_reader_ignores_unknown_chars() {
    // Random printable ASCII not in the keymap → Ignored, picker keeps reading.
    let it = items(&["a", "b"]);
    assert_eq!(run(b"xy\n", &it).expect("unknown then enter"), 0);
}

/// Verifies viewport calculations keep the selected item visible and centered
/// where possible for long lists.
#[test]
fn visible_window_centers_selection() {
    use crate::visible_window;
    // Fits entirely: full range.
    assert_eq!(visible_window(5, 2, 10), 0..5);
    // Overflow: window slides with selection.
    assert_eq!(visible_window(20, 0, 5), 0..5);
    assert_eq!(visible_window(20, 10, 5), 8..13);
    assert_eq!(visible_window(20, 19, 5), 15..20);
    assert_eq!(visible_window(20, 10, 1), 10..11);
}

fn line_text(line: &[tau_term_screen::style::Cell]) -> String {
    line.iter().map(|cell| cell.ch).collect()
}

/// Ensures one-row terminals use the compact frame so rendering stays within
/// the reported terminal height.
#[test]
fn one_row_terminal_uses_compact_frame() {
    let it = items(&["one", "two"]);
    let (lines, cursor_row) = picker_lines("pick", &it, 1, 80, 1);

    assert_eq!(lines.len(), 1);
    assert_eq!(cursor_row, 0);
    assert_eq!(line_text(&lines[0]), "> two — ? pick");
}

/// Ensures compact rendering truncates around the selected item first so tiny
/// terminals still show the active choice.
#[test]
fn compact_frame_prioritizes_selected_item_when_truncated() {
    let it = items(&["one", "selected-item"]);
    let (lines, cursor_row) = picker_lines("very long prompt", &it, 1, 8, 1);

    assert_eq!(cursor_row, 0);
    assert_eq!(line_text(&lines[0]), "> selec…");
}

/// Verifies normal-height rendering preserves every row's marker and bounded
/// text while positioning the cursor on the selected enabled item.
#[test]
fn normal_terminal_uses_prompt_plus_items() {
    let it = vec![
        PickerItem::enabled("ordinary"),
        PickerItem::disabled("unavailable"),
        PickerItem::enabled("selected label"),
    ];
    let (lines, cursor_row) = picker_lines("pick", &it, 2, 12, 4);

    assert_eq!(cursor_row, 3);
    assert_eq!(
        lines.iter().map(|line| line_text(line)).collect::<Vec<_>>(),
        ["? pick", "  ordinary", "X unavailab…", "> selected …"]
    );
}

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("writer buffer poisoned").clone()
    }
}

impl io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("writer buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingRawModeGuard {
    restore_error: Option<io::Error>,
}

impl RawModeCleanup for FailingRawModeGuard {
    fn restore_raw_mode(&mut self) -> io::Result<()> {
        match self.restore_error.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

struct FailsAfterFirstFlush {
    flushed_once: bool,
}

impl io::Write for FailsAfterFirstFlush {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.flushed_once {
            Err(io::Error::other("synthetic cleanup error"))
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed_once = true;
        Ok(())
    }
}

/// Ensures resize events immediately redraw with the new width so truncation
/// and layout update before the next keypress.
#[test]
fn resize_event_redraws_without_waiting_for_key_resample() {
    let it = items(&["very long item label"]);
    let writer = SharedWriter::default();
    let output = writer.clone();
    let mut events = VecDeque::from([
        PickerEvent::Resize {
            width: 8,
            height: 3,
        },
        PickerEvent::Key(PickerKey::Enter),
    ]);
    let mut size_samples = 0;
    let picked = pick_with_event_reader(
        "choose a thing",
        &it,
        writer,
        || Ok(events.pop_front().expect("test event available")),
        || {
            size_samples += 1;
            (40, 5)
        },
    )
    .expect("picker should accept after resize");

    assert_eq!(picked, 0);
    assert_eq!(
        size_samples, 1,
        "resize events should use their reported dimensions rather than resampling"
    );
    let bytes = output.bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("? choos…") && text.contains("> very …"),
        "resized redraw should contain the narrow prompt and item rows: {text:?}"
    );
}

/// Ensures zero-sized resize reports preserve the current dimensions and redraw
/// immediately without sampling ambient terminal state again.
#[test]
fn zero_resize_keeps_current_size_without_resampling_terminal() {
    let it = items(&["first row", "second row"]);
    let writer = SharedWriter::default();
    let output = writer.clone();
    let mut events = VecDeque::from([
        PickerEvent::Resize {
            width: 0,
            height: 0,
        },
        PickerEvent::Key(PickerKey::Enter),
    ]);
    let mut size_samples = 0;
    let picked = pick_with_event_reader(
        "distinct prompt",
        &it,
        writer,
        || Ok(events.pop_front().expect("test event available")),
        || {
            size_samples += 1;
            (12, 3)
        },
    )
    .expect("picker should accept after zero-sized resize");

    assert_eq!(picked, 0);
    assert_eq!(
        size_samples, 1,
        "zero-sized resize should retain the initial terminal dimensions"
    );
    let bytes = output.bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        text.matches("? distinct …").count(),
        2,
        "initial and zero-resize frames should both use the original width and height: {text:?}"
    );
    assert_eq!(
        text.matches("  second row").count(),
        2,
        "initial and zero-resize frames should both retain the second item row: {text:?}"
    );
}

/// Ensures user cancellation also clears the frame, covering the cancellation
/// cleanup path separately from I/O errors.
#[test]
fn picker_clears_frame_on_user_cancel() {
    // Cancellation exits through a different path than input errors; keep
    // cleanup covered so aborted prompts do not leave picker rows on screen.
    let it = items(&["one", "two"]);
    let writer = SharedWriter::default();
    let output = writer.clone();
    let err = pick_with_event_reader(
        "pick",
        &it,
        writer,
        || Ok(PickerEvent::Key(PickerKey::Cancelled)),
        || (40, 5),
    )
    .expect_err("user cancellation should propagate");

    assert!(matches!(err, PickerError::Cancelled));
    let bytes = output.bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("[J"), "cleanup should clear frame: {text:?}");
}

/// Ensures cancellation preserves the user-facing cancellation error even when
/// best-effort cleanup fails, matching the public error contract.
#[test]
fn cancel_cleanup_failure_does_not_replace_cancel_error() {
    let it = items(&["one", "two"]);
    let err = pick_with_event_reader(
        "pick",
        &it,
        FailsAfterFirstFlush {
            flushed_once: false,
        },
        || Ok(PickerEvent::Key(PickerKey::Cancelled)),
        || (40, 5),
    )
    .expect_err("user cancellation should remain visible");

    assert!(matches!(err, PickerError::Cancelled));
}

/// Ensures input errors preserve their original source even when best-effort
/// cleanup fails, so diagnostics are not replaced by cleanup noise.
#[test]
fn input_cleanup_failure_does_not_replace_input_error() {
    let it = items(&["one", "two"]);
    let err = pick_with_event_reader(
        "pick",
        &it,
        FailsAfterFirstFlush {
            flushed_once: false,
        },
        || Err(io::Error::other("synthetic input error")),
        || (40, 5),
    )
    .expect_err("input error should remain visible");

    match err {
        PickerError::Io(source) => assert_eq!(source.to_string(), "synthetic input error"),
        other => panic!("expected input IO error, got {other:?}"),
    }
}

/// Ensures successful selection reports cleanup failures instead of returning
/// a selected index whose picker frame may not have been cleared.
#[test]
fn selection_cleanup_failure_is_reported() {
    let it = items(&["one", "two"]);
    let err = pick_with_event_reader(
        "pick",
        &it,
        FailsAfterFirstFlush {
            flushed_once: false,
        },
        || Ok(PickerEvent::Key(PickerKey::Enter)),
        || (40, 5),
    )
    .expect_err("selection cleanup failure should be reported");

    match err {
        PickerError::Io(source) => assert_eq!(source.to_string(), "synthetic cleanup error"),
        other => panic!("expected cleanup IO error, got {other:?}"),
    }
}

/// Ensures raw-mode-owning picker calls report restoration failure instead of
/// returning a successful selection while the terminal may still be raw.
#[test]
fn raw_mode_restore_failure_replaces_successful_selection() {
    let it = items(&["one", "two"]);
    let err = pick_with_raw_mode(
        "pick",
        &it,
        Vec::<u8>::new(),
        || {
            Ok(FailingRawModeGuard {
                restore_error: Some(io::Error::other("synthetic raw restore error")),
            })
        },
        || Ok(PickerEvent::Key(PickerKey::Enter)),
        || (40, 5),
    )
    .expect_err("raw restore failure should be reported");

    match err {
        PickerError::Io(source) => assert_eq!(source.to_string(), "synthetic raw restore error"),
        other => panic!("expected raw restore IO error, got {other:?}"),
    }
}

/// Ensures raw-mode restoration errors are still surfaced after cancellation so
/// callers know terminal ownership may not have been released cleanly.
#[test]
fn raw_mode_restore_failure_replaces_cancel_error() {
    let it = items(&["one", "two"]);
    let err = pick_with_raw_mode(
        "pick",
        &it,
        Vec::<u8>::new(),
        || {
            Ok(FailingRawModeGuard {
                restore_error: Some(io::Error::other("synthetic raw restore error")),
            })
        },
        || Ok(PickerEvent::Key(PickerKey::Cancelled)),
        || (40, 5),
    )
    .expect_err("raw restore failure should be reported");

    match err {
        PickerError::Io(source) => assert_eq!(source.to_string(), "synthetic raw restore error"),
        other => panic!("expected raw restore IO error, got {other:?}"),
    }
}
