use crate::{CursorShape, Event, KeyCode, KeyEvent, KeyModifiers, RawEvent, Term};

/// Normalization precedes the byte threshold; only completed references enter
/// the editor, and cancellation preserves the original draft and cursor.
#[test]
fn paste_threshold_normalizes_before_interception_and_cancel_preserves_draft() {
    let (term, handle, input) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    handle.enable_paste_uploads(8192);
    handle.set_buffer("before after".to_owned(), 7);
    let small = "\r\n".repeat(4095);
    input.send(RawEvent::Paste(small)).expect("paste input");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    let original = handle.get_buffer();
    let cursor = handle.get_cursor();
    input
        .send(RawEvent::Paste("é".repeat(4096)))
        .expect("paste input");
    let Event::PasteUpload { id, text } = term.get_next_event().expect("upload event") else {
        panic!("upload")
    };
    assert_eq!(text.len(), 8192);
    assert_eq!(handle.get_buffer(), original);
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("cancel key");
    assert!(
        matches!(term.get_next_event().expect("cancel event"), Event::PasteCancelled { id: actual } if actual == id)
    );
    assert_eq!(handle.get_buffer(), original);
    assert_eq!(handle.get_cursor(), cursor);
}

/// Editing, bindings, external draft replacement, and submit cannot race an
/// upload; only the matching completion inserts ordinary editable text.
#[test]
fn paste_upload_serializes_edits_and_rejects_late_completions() {
    let (term, handle, input) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    handle.enable_paste_uploads(8);
    handle.set_buffer("ab".to_owned(), 1);
    input
        .send(RawEvent::Paste("long\r\npaste".to_owned()))
        .expect("paste input");
    let Event::PasteUpload { id, text } = term.get_next_event().expect("upload event") else {
        panic!("upload")
    };
    assert_eq!(&*text, "long\npaste");
    for code in [KeyCode::Char('x'), KeyCode::Up, KeyCode::Enter] {
        input
            .send(RawEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .expect("blocked key");
    }
    handle.set_buffer("replacement".to_owned(), 0);
    assert!(!handle.set_buffer_if_revision(
        handle.get_buffer_revision(),
        "replacement".to_owned(),
        0
    ));
    input
        .send(RawEvent::Paste("second paste".to_owned()))
        .expect("concurrent paste");
    assert!(matches!(
        term.get_next_event().expect("busy notice"),
        Event::Notice(_)
    ));
    assert_eq!(handle.get_buffer(), "ab");
    handle.finish_paste_upload(id + 1, Ok("WRONG".to_owned()));
    handle.finish_paste_upload(id, Ok("<tau-artifact:key>".to_owned()));
    assert!(matches!(
        term.get_next_event().expect("completion event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "a<tau-artifact:key>b");
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )))
        .expect("edit reference");
    assert!(matches!(
        term.get_next_event().expect("edit event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "a<tau-artifact:keyb");
}

/// Failure retains original bytes outside the prompt; retry gets fresh
/// identity, and an old attempt cannot complete the retry or resurrect a
/// canceled paste.
#[test]
fn paste_failure_retry_and_cancel_keep_source_out_of_draft() {
    let (term, handle, input) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    handle.enable_paste_uploads(8);
    handle.set_buffer("draft".to_owned(), 2);
    input
        .send(RawEvent::Paste("secret wall".to_owned()))
        .expect("paste input");
    let Event::PasteUpload { id, .. } = term.get_next_event().expect("upload event") else {
        panic!("upload")
    };
    handle.finish_paste_upload(id, Err("unavailable".to_owned()));
    assert!(matches!(
        term.get_next_event().expect("failure notice"),
        Event::Notice(_)
    ));
    assert_eq!(handle.get_buffer(), "draft");
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("retry key");
    let Event::PasteUpload { id: retry, text } = term.get_next_event().expect("retry event") else {
        panic!("retry")
    };
    assert_ne!(id, retry);
    assert_eq!(&*text, "secret wall");
    handle.finish_paste_upload(id, Ok("WRONG".to_owned()));
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("cancel key");
    assert!(
        matches!(term.get_next_event().expect("cancel event"), Event::PasteCancelled { id: actual } if actual == retry)
    );
    handle.finish_paste_upload(retry, Ok("WRONG".to_owned()));
    input
        .send(RawEvent::Paste("ok".to_owned()))
        .expect("small paste");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "drokaft");
}
