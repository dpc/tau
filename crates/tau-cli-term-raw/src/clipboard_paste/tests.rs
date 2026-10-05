use std::io;
use std::sync::mpsc::Sender;

use super::*;

/// Matches Kitty's read-response grammar and WezTerm's ClipboardResponse
/// serializer: only DATA carries a payload separator.
fn packet(status: &str, id: Option<&str>, mime: Option<&str>, bytes: &[u8]) -> Vec<u8> {
    let mut metadata = format!("5522;type=read:status={status}");
    if let Some(id) = id {
        metadata.push_str(&format!(":id={id}"))
    }
    if let Some(mime) = mime {
        metadata.push_str(&format!(":mime={}", STANDARD.encode(mime)))
    }
    if status == "DATA" {
        format!("{metadata};{}", STANDARD.encode(bytes)).into_bytes()
    } else {
        assert!(bytes.is_empty(), "metadata-only response has no bytes");
        metadata.into_bytes()
    }
}

/// Literal WezTerm offer/control frames must open an acquisition and release
/// PNG bytes only after the matching metadata-only DONE, not on DATA.
#[test]
fn wezterm_metadata_only_controls_complete_png_read() {
    let now = Instant::now();
    let mut owner = ClipboardPaste::default();
    owner.probe(now);
    owner.mode_report(true, now);
    owner.receive(b"5522;type=read:status=OK:pw=c2VjcmV0", now, false);
    assert!(owner.busy());
    owner.receive(
        b"5522;type=read:status=DATA:mime=Lg==;aW1hZ2UvcG5n",
        now,
        false,
    );
    let request = owner.receive(b"5522;type=read:status=DONE", now, false);
    assert!(!request.output.is_empty());
    let id = read_id(&owner);
    assert!(
        owner
            .receive(
                format!("5522;type=read:id={id}:status=OK").as_bytes(),
                now,
                false,
            )
            .content
            .is_none()
    );
    assert!(
        owner
            .receive(
                format!("5522;type=read:id={id}:status=DATA:mime=aW1hZ2UvcG5n;iVBORw0KGgo=")
                    .as_bytes(),
                now,
                false,
            )
            .content
            .is_none()
    );
    assert!(
        owner
            .receive(b"5522;type=read:id=wrong:status=DONE", now, false)
            .content
            .is_none()
    );
    let effects = owner.receive(
        format!("5522;type=read:id={id}:status=DONE").as_bytes(),
        now,
        false,
    );
    assert!(
        matches!(effects.content, Some(PasteContent::Png(ref bytes)) if &**bytes == b"\x89PNG\r\n\x1a\n")
    );
}

/// A missing DATA delimiter must not become an empty chunk or artifact, and
/// unknown metadata-only statuses must not complete an active read.
#[test]
fn metadata_only_data_and_unknown_status_discard_acquisition() {
    for status in ["DATA:mime=aW1hZ2UvcG5n", "UNKNOWN"] {
        let (mut owner, now, _) = offered(b"image/png");
        let id = read_id(&owner);
        owner.receive(&packet("OK", Some(&id), None, b""), now, false);
        let effects = owner.receive(
            format!("5522;type=read:id={id}:status={status}").as_bytes(),
            now,
            false,
        );
        assert!(effects.notice.is_some());
        assert!(effects.content.is_none());
        assert!(!owner.busy());
    }
}

/// Enables a probed owner and supplies a complete unsolicited MIME inventory.
fn offered(mimes: &[u8]) -> (ClipboardPaste, Instant, String) {
    let now = Instant::now();
    let mut owner = ClipboardPaste::default();
    owner.probe(now);
    assert_eq!(owner.mode_report(true, now).output, b"\x1b[?5522h");
    owner.receive(&packet("OK", None, None, b""), now, false);
    owner.receive(&packet("DATA", None, Some("."), mimes), now, false);
    let request = owner.receive(&packet("DONE", None, None, b""), now, false);
    let request = String::from_utf8(request.output).expect("ASCII content request");
    (owner, now, request)
}

/// PNG wins even when plain text is also available; chunks decode independently
/// and only matching DONE releases completed bytes.
#[test]
fn png_chunks_complete_only_after_correlated_done() {
    let (mut owner, now, request) = offered(b"text/plain image/png text/uri-list");
    assert!(request.ends_with(&format!(";{}\x1b\\", STANDARD.encode("image/png"))));
    let read_id = read_id(&owner);
    let id = read_id.as_str();
    owner.receive(&packet("OK", Some(id), None, b""), now, false);
    assert!(
        owner
            .receive(
                &packet("DATA", Some(id), Some("image/png"), b"abc"),
                now,
                false
            )
            .content
            .is_none()
    );
    owner.receive(
        &packet("DATA", Some(id), Some("image/png"), b"d"),
        now,
        false,
    );
    assert!(
        owner
            .receive(&packet("DONE", Some("stale"), None, b""), now, false)
            .content
            .is_none()
    );
    let effects = owner.receive(&packet("DONE", Some(id), None, b""), now, false);
    assert!(matches!(effects.content, Some(PasteContent::Png(ref bytes)) if &**bytes == b"abcd"));
    assert!(!owner.busy());
}

/// UTF-8 text fallback uses a real read transaction; a URI inventory never
/// becomes filesystem authority.
#[test]
fn plaintext_and_unsupported_mime_are_explicit() {
    let (mut owner, now, request) = offered(b"text/plain");
    let id = read_id(&owner);
    assert!(request.ends_with(&format!(";{}\x1b\\", STANDARD.encode("text/plain"))));
    owner.receive(&packet("OK", Some(&id), None, b""), now, false);
    owner.receive(
        &packet("DATA", Some(&id), Some("text/plain"), "é\n".as_bytes()),
        now,
        false,
    );
    let effects = owner.receive(&packet("DONE", Some(&id), None, b""), now, false);
    assert!(matches!(effects.content, Some(PasteContent::Text(ref text)) if &**text == "é\n"));
    let (owner, _, request) = offered(b"text/uri-list");
    assert!(request.is_empty());
    assert!(!owner.busy());
}

/// Missing capabilities and incomplete reads have fixed deadlines; late packets
/// cannot resurrect discarded data or enable an expired capability probe.
#[test]
fn timeouts_reset_and_stale_ids_are_bounded() {
    let mut owner = ClipboardPaste::default();
    let now = Instant::now();
    owner.probe(now);
    owner.expire(now + Duration::from_secs(2));
    assert!(
        owner
            .mode_report(true, now + Duration::from_secs(2))
            .output
            .is_empty()
    );
    let (mut owner, now, _) = offered(b"image/png");
    let id = read_id(&owner);
    owner.receive(&packet("OK", Some(&id), None, b""), now, false);
    owner.receive(
        &packet("DATA", Some(&id), Some("image/png"), b"partial"),
        now,
        false,
    );
    assert!(owner.expire(now + Duration::from_secs(16)).notice.is_some());
    assert!(
        owner
            .receive(&packet("DONE", Some(&id), None, b""), now, false)
            .content
            .is_none()
    );
    owner.reset();
    owner.probe(now);
    owner.mode_report(true, now);
    owner.receive(&packet("OK", None, None, b""), now, false);
    owner.receive(&packet("DATA", None, Some("."), b"image/png"), now, false);
    let request = owner.receive(&packet("DONE", None, None, b""), now, false);
    assert!(
        String::from_utf8(request.output)
            .expect("ASCII content request")
            .contains(&format!("id=tau-paste-{}-2", owner.identity))
    );
}

/// Read errors replace OK and second gestures do not destroy an active read.
#[test]
fn read_errors_and_busy_preserve_transaction_identity() {
    for status in ["EPERM", "EBUSY", "ENOSYS"] {
        let (mut owner, now, _) = offered(b"image/png");
        let id = read_id(&owner);
        assert!(
            owner
                .receive(&packet("OK", None, None, b""), now, false)
                .notice
                .is_some()
        );
        assert!(owner.busy());
        let error = owner.receive(&packet(status, Some(&id), None, b""), now, false);
        assert!(error.notice.is_some());
        assert!(!owner.busy());
    }
}

/// Invalid base64, wrong MIME, excess chunks and aggregate overflow must never
/// publish partial artifacts.
#[test]
fn invalid_chunks_and_aggregate_overflow_discard_all_bytes() {
    for (mime, bytes) in [("image/png", vec![0; 4097]), ("text/plain", vec![0; 1])] {
        let (mut owner, now, _) = offered(b"image/png");
        let id = read_id(&owner);
        owner.receive(&packet("OK", Some(&id), None, b""), now, false);
        assert!(
            owner
                .receive(&packet("DATA", Some(&id), Some(mime), &bytes), now, false)
                .notice
                .is_some()
        );
        assert!(!owner.busy());
    }
    let (mut owner, now, _) = offered(b"image/png");
    let id = read_id(&owner);
    owner.receive(&packet("OK", Some(&id), None, b""), now, false);
    let chunk = packet("DATA", Some(&id), Some("image/png"), &[0; MAX_CHUNK]);
    for _ in 0..MAX_BYTES / MAX_CHUNK {
        assert!(owner.receive(&chunk, now, false).notice.is_none());
    }
    assert!(owner.receive(&chunk, now, false).notice.is_some());
    assert!(!owner.busy());
}

/// Location/grant/name metadata are preserved and MIME requests use the
/// payload.
#[test]
fn primary_grant_is_preserved_without_ambient_reads() {
    let now = Instant::now();
    let mut owner = ClipboardPaste::default();
    owner.probe(now);
    owner.mode_report(true, now);
    owner.receive(
        b"5522;type=read:status=OK:loc=primary:pw=c2VjcmV0;",
        now,
        false,
    );
    owner.receive(&packet("DATA", None, Some("."), b"image/png"), now, false);
    let request = owner.receive(&packet("DONE", None, None, b""), now, false);
    let request = String::from_utf8(request.output).expect("ASCII content request");
    assert!(request.contains(":loc=primary:pw=c2VjcmV0:name=UGFzdGUgZXZlbnQ="));
    assert!(!request.contains(":mime="));
}

/// Pumps an offer through the actual input owner, then returns its fresh read
/// ID.
fn begin_read(
    term: &crate::Term,
    handle: &crate::TermHandle,
    input: &Sender<crate::RawEvent>,
    mime: &str,
) -> String {
    handle.lock().editor.clipboard.probe(Instant::now());
    input
        .send(crate::RawEvent::ClipboardModeReport(true))
        .expect("inject supported mode report");
    for body in [
        packet("OK", None, None, b""),
        packet("DATA", None, Some("."), mime.as_bytes()),
        packet("DONE", None, None, b""),
    ] {
        input
            .send(crate::RawEvent::Osc(body))
            .expect("inject MIME offer");
    }
    input
        .send(crate::RawEvent::Resize(80, 24))
        .expect("inject pump marker");
    assert!(matches!(
        term.get_next_event().expect("pump MIME offer"),
        crate::Event::Resize { .. }
    ));
    read_id(&handle.lock().editor.clipboard)
}

/// Real input-owner plumbing freezes acquisition and PNG upload, retries only
/// captured upload bytes, rejects stale completions, and never submits a
/// prompt.
#[test]
fn png_input_owner_freezes_draft_and_preserves_upload_retry_source() {
    use crate::{CursorShape, Event, KeyCode, KeyEvent, KeyModifiers, RawEvent, Term};
    let (term, handle, input) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    handle.set_buffer("before after".into(), 7);
    let id = begin_read(&term, &handle, &input, "image/png");
    handle.set_buffer("WRONG".into(), 0);
    assert_eq!(handle.get_buffer(), "before after");
    for key in [KeyCode::Char('x'), KeyCode::Enter] {
        input
            .send(RawEvent::Key(KeyEvent::new(key, KeyModifiers::NONE)))
            .expect("inject blocked key");
    }
    for body in [
        packet("OK", Some(&id), None, b""),
        packet(
            "DATA",
            Some(&id),
            Some("image/png"),
            b"\x89PNG\r\n\x1a\nopaque",
        ),
        packet("DONE", Some("wrong"), None, b""),
        packet("DONE", Some(&id), None, b""),
    ] {
        input
            .send(RawEvent::Osc(body))
            .expect("inject PNG response");
    }
    let Event::PastePngUpload { id: upload, bytes } =
        term.get_next_event().expect("complete PNG acquisition")
    else {
        panic!("PNG must upload, never submit or append");
    };
    assert_eq!(&*bytes, b"\x89PNG\r\n\x1a\nopaque");
    assert_eq!(handle.get_buffer(), "before after");
    handle.finish_paste_upload(upload, Err("test failure".into()));
    assert!(matches!(
        term.get_next_event().expect("upload failure notice"),
        Event::Notice(_)
    ));
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("inject explicit upload retry");
    let Event::PastePngUpload {
        id: retry,
        bytes: retried,
    } = term.get_next_event().expect("PNG upload retry")
    else {
        panic!("retry must retain PNG");
    };
    assert_ne!(upload, retry);
    assert!(Arc::ptr_eq(&bytes, &retried));
    handle.finish_paste_upload(upload, Ok("WRONG".into()));
    handle.finish_paste_upload(retry, Ok("<tau-artifact:key>".into()));
    assert!(matches!(
        term.get_next_event().expect("matching upload completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "before <tau-artifact:key>after");
    assert_eq!(handle.get_cursor(), 25);
}

/// Completed5522 text follows the same normalization and byte threshold as2004.
#[test]
fn native_text_keeps_legacy_threshold_and_newline_semantics() {
    use crate::{CursorShape, Event, RawEvent, Term};
    for (text, uploaded) in [("x\r\ny", false), ("long\r\npaste", true)] {
        let (term, handle, input) =
            Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
        handle.enable_paste_uploads(8);
        handle.set_buffer("draft".into(), 2);
        let id = begin_read(&term, &handle, &input, "text/plain");
        for body in [
            packet("OK", Some(&id), None, b""),
            packet("DATA", Some(&id), Some("text/plain"), text.as_bytes()),
            packet("DONE", Some(&id), None, b""),
        ] {
            input
                .send(RawEvent::Osc(body))
                .expect("inject text response");
        }
        match term.get_next_event().expect("complete text acquisition") {
            Event::PasteUpload { text: source, .. } if uploaded => {
                assert_eq!(&*source, "long\npaste");
                assert_eq!(handle.get_buffer(), "draft");
            }
            Event::BufferChanged if !uploaded => assert_eq!(handle.get_buffer(), "drx\nyaft"),
            _ => panic!("text must use existing threshold, not submit"),
        }
    }
}

/// Cancel/focus loss retire partial bytes; late DONE cannot append or upload
/// them.
#[test]
fn acquisition_cancel_and_focus_loss_retire_incomplete_bytes() {
    use crate::{CursorShape, Event, KeyCode, KeyEvent, KeyModifiers, RawEvent, Term};
    for focus_loss in [false, true] {
        let (term, handle, input) =
            Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
        handle.set_buffer("draft".into(), 2);
        let id = begin_read(&term, &handle, &input, "image/png");
        input
            .send(RawEvent::Osc(packet("OK", Some(&id), None, b"")))
            .expect("inject read OK");
        input
            .send(RawEvent::Osc(packet(
                "DATA",
                Some(&id),
                Some("image/png"),
                b"partial",
            )))
            .expect("inject partial DATA");
        if focus_loss {
            input
                .send(RawEvent::FocusChanged { focused: false })
                .expect("inject focus loss");
            assert!(matches!(
                term.get_next_event().expect("focus event"),
                Event::FocusChanged { focused: false }
            ));
        } else {
            input
                .send(RawEvent::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )))
                .expect("inject acquisition cancellation");
        }
        assert!(matches!(
            term.get_next_event().expect("acquisition discard notice"),
            Event::Notice(_)
        ));
        input
            .send(RawEvent::Osc(packet("DONE", Some(&id), None, b"")))
            .expect("inject stale DONE");
        input
            .send(RawEvent::Resize(80, 24))
            .expect("inject pump marker");
        assert!(matches!(
            term.get_next_event().expect("stale DONE ignored"),
            Event::Resize { .. }
        ));
        assert_eq!(handle.get_buffer(), "draft");
        assert_eq!(handle.get_cursor(), 2);
    }
}

/// Mismatched grants and malformed chunks never result in content admission.
#[test]
fn grant_mismatch_and_invalid_base64_discard_incomplete_state() {
    let now = Instant::now();
    let mut owner = ClipboardPaste::default();
    owner.probe(now);
    owner.mode_report(true, now);
    owner.receive(b"5522;type=read:status=OK:pw=b25l;", now, false);
    let effects = owner.receive(
        b"5522;type=read:status=DATA:mime=Lg==:pw=dHdv;aW1hZ2UvcG5n",
        now,
        false,
    );
    assert!(effects.notice.is_some());
    assert!(!owner.busy());
    let (mut owner, now, _) = offered(b"image/png");
    let id = read_id(&owner);
    owner.receive(&packet("OK", Some(&id), None, b""), now, false);
    let malformed = format!("5522;type=read:status=DATA:id={id}:mime=aW1hZ2UvcG5n;%%%%");
    assert!(
        owner
            .receive(malformed.as_bytes(), now, false)
            .notice
            .is_some()
    );
    assert!(!owner.busy());
}

/// Generic native events reach the existing input owner without logging bytes
/// or turning unrelated reports into editor keystrokes.
#[test]
fn crossterm_response_mapping_uses_existing_reader() {
    use crossterm::event::{Event, ModeReport, ModeStatus};
    let mut events = [
        Event::ModeReport(ModeReport {
            mode: 2004,
            status: ModeStatus::Set,
        }),
        Event::ModeReport(ModeReport {
            mode: 5522,
            status: ModeStatus::Reset,
        }),
        Event::Osc(b"5522;type=read:status=OK;".to_vec()),
    ]
    .into_iter();
    assert!(matches!(
        crate::read_real_raw_event(
            || Ok(events.next().expect("mode report fixture")),
            || Ok((80, 24)),
        )
        .expect("native mode mapping"),
        crate::RawEvent::ClipboardModeReport(true)
    ));
    assert!(matches!(crate::read_real_raw_event(
        || Ok(events.next().expect("OSC fixture")), || Ok((80, 24)),
    ).expect("native OSC mapping"), crate::RawEvent::Osc(ref body) if body == b"5522;type=read:status=OK;"));
}

/// Output controls belong to the redraw writer, not a separate stdout writer;
/// an injected flush failure fail-stops the attachment rather than losing a
/// read.
#[test]
fn clipboard_controls_use_output_owner_and_propagate_failure() {
    use crate::{CursorShape, Event, RawEvent, Term};
    /// Accepts writes but rejects flushes to exercise the retained output
    /// failure.
    struct RejectingWriter;
    impl std::io::Write for RejectingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(io::Error::other("injected flush failure"))
        }
    }
    let (term, handle, input) =
        Term::new_virtual(80, 24, "> ", Box::new(RejectingWriter), CursorShape::Bar);
    handle.lock().editor.clipboard.probe(Instant::now());
    input
        .send(RawEvent::ClipboardModeReport(true))
        .expect("inject supported mode report");
    assert!(term.get_next_event().is_err());
    assert!(handle.lock().terminal.output_failure.is_some());
    assert!(!matches!(
        term.get_next_event(),
        Ok(Event::PastePngUpload { .. })
    ));
}

/// Extracts the correlated request identity after successful offer admission.
fn read_id(owner: &ClipboardPaste) -> String {
    owner
        .transfer
        .as_ref()
        .expect("active content read")
        .id
        .clone()
        .expect("correlated content read identity")
}
