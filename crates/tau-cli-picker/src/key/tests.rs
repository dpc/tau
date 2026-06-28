use std::collections::VecDeque;
use std::io;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    LogicalKey, PickerEvent, PickerKey, logical_to_action, read_byte_key,
    terminal_event_to_picker_event, terminal_key_to_logical,
};

struct ScriptedReader {
    steps: VecDeque<io::Result<Option<u8>>>,
}

impl ScriptedReader {
    fn new(steps: impl IntoIterator<Item = io::Result<Option<u8>>>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
        }
    }
}

impl io::Read for ScriptedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.steps.pop_front() {
            Some(Ok(Some(byte))) => {
                buf[0] = byte;
                Ok(1)
            }
            Some(Ok(None)) | None => Ok(0),
            Some(Err(err)) => Err(err),
        }
    }
}

/// Verifies the central logical-key mapping so terminal and byte-stream readers
/// continue to share the same controls.
#[test]
fn logical_mapping_is_single_source_of_truth() {
    assert_eq!(logical_to_action(LogicalKey::Up), PickerKey::Up);
    assert_eq!(logical_to_action(LogicalKey::Down), PickerKey::Down);
    assert_eq!(logical_to_action(LogicalKey::Tab), PickerKey::Down);
    assert_eq!(logical_to_action(LogicalKey::BackTab), PickerKey::Up);
    assert_eq!(logical_to_action(LogicalKey::Enter), PickerKey::Enter);
    assert_eq!(logical_to_action(LogicalKey::Esc), PickerKey::Cancelled);
    assert_eq!(logical_to_action(LogicalKey::CtrlC), PickerKey::Cancelled);
    assert_eq!(logical_to_action(LogicalKey::CtrlD), PickerKey::Cancelled);
    assert_eq!(logical_to_action(LogicalKey::Char('j')), PickerKey::Down);
    assert_eq!(logical_to_action(LogicalKey::Char('k')), PickerKey::Up);
    assert_eq!(
        logical_to_action(LogicalKey::Char('q')),
        PickerKey::Cancelled
    );
    assert_eq!(logical_to_action(LogicalKey::Char(' ')), PickerKey::Ignored);
}

/// Protects terminal-event Ctrl-C/Ctrl-D decoding so terminal input keeps the
/// same cancellation behavior as the byte-stream test reader.
#[test]
fn terminal_control_chars_decode_to_logical_cancellation_keys() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

    assert_eq!(terminal_key_to_logical(ctrl_c), LogicalKey::CtrlC);
    assert_eq!(terminal_key_to_logical(ctrl_d), LogicalKey::CtrlD);
}

/// Ensures only documented plain character shortcuts are honored; unrelated
/// Ctrl/Alt modified characters should not navigate or cancel the picker.
#[test]
fn terminal_modified_character_shortcuts_are_ignored() {
    for key in ['j', 'k', 'q'] {
        let ctrl_key = KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL);
        let alt_key = KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT);

        assert_eq!(terminal_key_to_logical(ctrl_key), LogicalKey::Unknown);
        assert_eq!(terminal_key_to_logical(alt_key), LogicalKey::Unknown);
    }
}

/// Ensures transient read interruptions inside an escape sequence are retried
/// instead of turning a valid arrow-key sequence into a picker I/O failure.
#[test]
fn byte_reader_retries_interrupted_escape_sequence_reads() {
    let interrupted = io::Error::from(io::ErrorKind::Interrupted);
    let mut reader = ScriptedReader::new([
        Ok(Some(0x1b)),
        Err(interrupted),
        Ok(Some(b'[')),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Ok(Some(b'B')),
    ]);

    assert_eq!(
        read_byte_key(&mut reader).expect("down arrow"),
        PickerKey::Down
    );
}

/// Protects terminal-event decoding for every documented non-control key so
/// crossterm input stays in parity with byte-stream controls.
#[test]
fn terminal_documented_controls_decode_to_logical_keys() {
    let cases = [
        (
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            LogicalKey::Up,
        ),
        (
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            LogicalKey::Down,
        ),
        (
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            LogicalKey::Tab,
        ),
        (
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            LogicalKey::BackTab,
        ),
        (
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            LogicalKey::Enter,
        ),
        (
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            LogicalKey::Esc,
        ),
        (
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            LogicalKey::Char('j'),
        ),
        (
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            LogicalKey::Char('k'),
        ),
        (
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            LogicalKey::Char('q'),
        ),
        (
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            LogicalKey::Char(' '),
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(terminal_key_to_logical(event), expected);
    }
}

/// Ensures the terminal event adapter ignores key releases/repeats so enhanced
/// keyboard reporting does not double-fire picker actions.
#[test]
fn terminal_event_adapter_filters_non_press_keys() {
    let mut release = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    let mut repeat = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    repeat.kind = KeyEventKind::Repeat;

    assert_eq!(terminal_event_to_picker_event(Event::Key(release)), None);
    assert_eq!(terminal_event_to_picker_event(Event::Key(repeat)), None);
}

/// Verifies terminal resize events are surfaced as picker events so the picker
/// can redraw immediately without waiting for another keypress.
#[test]
fn terminal_event_adapter_surfaces_resize_events() {
    assert_eq!(
        terminal_event_to_picker_event(Event::Resize(7, 3)),
        Some(PickerEvent::Resize {
            width: 7,
            height: 3
        })
    );
}

/// Ensures unrelated terminal events are ignored by the read loop instead of
/// being misinterpreted as navigation or cancellation.
#[test]
fn terminal_event_adapter_ignores_unrelated_events() {
    assert_eq!(terminal_event_to_picker_event(Event::FocusGained), None);
    assert_eq!(terminal_event_to_picker_event(Event::FocusLost), None);
}

/// Protects the positive terminal-event key path so key presses continue to use
/// the same logical key map as byte-stream input.
#[test]
fn terminal_event_adapter_maps_key_presses() {
    let press = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);

    assert_eq!(
        terminal_event_to_picker_event(Event::Key(press)),
        Some(PickerEvent::Key(PickerKey::Down))
    );
}
