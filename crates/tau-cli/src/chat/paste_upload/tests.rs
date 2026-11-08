use std::io::BufReader;
use std::os::unix::net::UnixStream;

use super::*;
use crate::chat::{UiIoMeter, UiWriter};

/// Arbitrary native audio bytes use the existing artifact wire and carry only
/// an untrusted media hint, not an invented source filename.
#[test]
fn opaque_audio_uses_existing_original_upload_and_safe_hint() {
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (_term, handle, _input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    let content = tau_cli_term::PasteContent::Opaque {
        bytes: Arc::from(&b"\0\xffMP3"[..]),
        mime_type: Arc::from("audio/mpeg"),
    };
    control.start_content("s1".parse().expect("session"), 1, content.clone(), handle);
    reply(&control, read_request(&mut reader), Ok(ArtifactValue::Done));
    let key = finish_original(&control, &mut reader, b"\0\xffMP3", "audio");
    assert_eq!(
        paste_reference(&key, &content),
        format!(
            "{} (mime_type: audio/mpeg)",
            tau_proto::artifact_reference(&key)
        )
    );
    control.stop();
    worker.join().expect("worker exit");
}

/// Builds two opaque originals with per-use metadata, not remote paths.
fn file_source() -> tau_cli_term::PasteContent {
    let mut bytes = b"TAUFILES\0".to_vec();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    for (name, mime, body) in [
        ("song.mp3", "audio/mpeg", &b"\0\xffMP3"[..]),
        ("café <ref>.dat", "", &b"second"[..]),
    ] {
        bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&(mime.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&(body.len() as u64).to_be_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(mime.as_bytes());
        bytes.extend_from_slice(body);
    }
    tau_cli_term::PasteContent::Files(Arc::new(
        tau_cli_term::ClipboardFiles::parse(bytes.into()).expect("fixture files"),
    ))
}

/// Drives one small original through the real worker's artifact request stream.
fn finish_original(
    control: &PasteUpload,
    reader: &mut tau_proto::HarnessInputReader<BufReader<UnixStream>>,
    bytes: &[u8],
    upload: &str,
) -> tau_proto::ArtifactKey {
    let begin = read_request(reader);
    assert!(
        matches!(begin.op, ArtifactOp::Begin { ref size, .. } if size.get() == bytes.len() as u64)
    );
    let ArtifactOp::Begin { ref identity, .. } = begin.op else {
        panic!("begin")
    };
    let identity = identity.clone();
    assert!(identity.mime_type().is_some());
    reply(
        control,
        begin,
        Ok(ArtifactValue::Upload {
            upload: upload.parse().expect("upload identity"),
        }),
    );
    let write = read_request(reader);
    assert!(
        matches!(write.op, ArtifactOp::Write { offset: 0, bytes: ref actual, .. } if actual == bytes)
    );
    reply(
        control,
        write,
        Ok(ArtifactValue::Written {
            next_offset: bytes.len() as u64,
        }),
    );
    let finalize = read_request(reader);
    assert!(matches!(finalize.op, ArtifactOp::Finalize { .. }));
    let key: tau_proto::ArtifactKey = identity.digest(bytes).to_hex()[..24]
        .to_owned()
        .parse()
        .expect("original key");
    reply(
        control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            identity,
            key: key.clone(),
            size: tau_proto::ArtifactSize::new(bytes.len() as u64).expect("original size"),
        })),
    );
    key
}

/// The worker uploads each file's body (not the envelope), retains the first
/// finalized original on second-file retry, and inserts all references
/// together. A frozen editor is established through its existing paste path;
/// file-specific acquisition/source retention has its own raw-input oracle.
#[test]
fn copied_files_upload_sequentially_and_retry_without_partial_prompt_insertion() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tau_cli_term_raw::{Event, RawEvent};
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (term, handle, input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    handle.enable_paste_uploads(1);
    handle.set_buffer("draft: ".into(), 7);
    input
        .send(RawEvent::Paste("freeze".into()))
        .expect("freeze editor");
    let Event::PasteUpload { id, .. } = term.get_next_event().expect("pending paste") else {
        panic!("pending paste");
    };
    let content = file_source();
    control.start_content(
        "s1".parse().expect("session"),
        id,
        content.clone(),
        handle.clone(),
    );
    let available = read_request(&mut reader);
    assert!(matches!(available.op, ArtifactOp::Available));
    reply(&control, available, Ok(ArtifactValue::Done));
    let first_key = finish_original(&control, &mut reader, b"\0\xffMP3", "first-file");
    assert_eq!(handle.get_buffer(), "draft: ");
    let begin = read_request(&mut reader);
    assert!(matches!(begin.op, ArtifactOp::Begin { ref size, .. } if size.get() == 6));
    let ArtifactOp::Begin { ref identity, .. } = begin.op else {
        panic!("begin")
    };
    let identity = identity.clone();
    reply(
        &control,
        begin,
        Ok(ArtifactValue::Upload {
            upload: "second-file".parse().expect("upload"),
        }),
    );
    let first_write = read_request(&mut reader);
    assert!(matches!(first_write.op, ArtifactOp::Write { ref bytes, .. } if bytes == b"second"));
    reply(
        &control,
        first_write.clone(),
        Err(tau_proto::ArtifactError::Busy),
    );
    assert!(matches!(
        term.get_next_event().expect("failure"),
        Event::Notice(_)
    ));
    assert_eq!(handle.get_buffer(), "draft: ");
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("retry");
    let Event::PasteUpload { id: retry_id, .. } = term.get_next_event().expect("retry") else {
        panic!("explicit retry");
    };
    control.start_content(
        "s1".parse().expect("session"),
        retry_id,
        content.clone(),
        handle.clone(),
    );
    let retry = read_request(&mut reader);
    assert_eq!(
        retry.op, first_write.op,
        "no repeated first-file upload or second Begin"
    );
    assert_ne!(retry.request_id, first_write.request_id);
    reply(
        &control,
        retry,
        Ok(ArtifactValue::Written { next_offset: 6 }),
    );
    let finalize = read_request(&mut reader);
    assert!(matches!(finalize.op, ArtifactOp::Finalize { .. }));
    let second_key = identity.digest(b"second").to_hex()[..24]
        .to_owned()
        .parse()
        .expect("key");
    reply(
        &control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            key: second_key,
            identity: identity.clone(),
            size: tau_proto::ArtifactSize::new(6).expect("size"),
        })),
    );
    assert!(matches!(
        term.get_next_event().expect("group completion"),
        Event::BufferChanged
    ));
    assert_eq!(
        handle.get_buffer(),
        format!(
            "draft: {}\n{}",
            paste_reference_at(&first_key, &content, 0),
            paste_reference_at(
                &identity.digest(b"second").to_hex()[..24]
                    .to_owned()
                    .parse()
                    .expect("key"),
                &content,
                1
            )
        ),
    );
    assert!(handle.get_buffer().contains("filename: caf___ref_.dat"));
    control.stop();
    worker.join().expect("worker exit");
}

/// Canceling after the first file finalized aborts only the current unfinished
/// upload; no partial group enters the draft and immutable originals are not
/// deleted as an invented rollback operation.
#[test]
fn copied_files_cancel_preserves_draft_and_aborts_only_unfinished_original() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tau_cli_term_raw::{Event, RawEvent};
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (term, handle, input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    handle.enable_paste_uploads(1);
    handle.set_buffer("draft".into(), 5);
    input
        .send(RawEvent::Paste("freeze".into()))
        .expect("freeze");
    let Event::PasteUpload { id, .. } = term.get_next_event().expect("pending") else {
        panic!("pending");
    };
    control.start_content(
        "s1".parse().expect("session"),
        id,
        file_source(),
        handle.clone(),
    );
    reply(&control, read_request(&mut reader), Ok(ArtifactValue::Done));
    finish_original(&control, &mut reader, b"\0\xffMP3", "first-file");
    reply(
        &control,
        read_request(&mut reader),
        Ok(ArtifactValue::Upload {
            upload: "second-file".parse().expect("upload"),
        }),
    );
    let outstanding = read_request(&mut reader);
    assert!(matches!(outstanding.op, ArtifactOp::Write { .. }));
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("discard");
    assert!(matches!(
        term.get_next_event().expect("cancel"),
        Event::PasteCancelled { .. }
    ));
    control.cancel(id);
    let abort = read_request(&mut reader);
    assert!(matches!(abort.op, ArtifactOp::Abort { upload } if upload.as_str() == "second-file"));
    assert_eq!(handle.get_buffer(), "draft");
    reply(
        &control,
        outstanding,
        Ok(ArtifactValue::Written { next_offset: 6 }),
    );
    control.stop();
    worker.join().expect("worker exit");
}

/// Both protocol states below 8.1 and absent version knowledge disable uploads.
#[test]
fn paste_upload_version_gate() {
    assert!(!supported(None));
    assert!(!supported(Some(tau_proto::ProtocolVersion::new(8, 0))));
    assert!(supported(Some(tau_proto::ProtocolVersion::new(8, 1))));
}

/// Completed clipboard artifacts carry their known media claim beside the
/// canonical key, without inventing an original filename from permission
/// labels.
#[test]
fn clipboard_artifact_reference_preserves_png_and_normalized_text_hints() {
    let key = blake3::hash(b"original").to_hex()[..24]
        .to_owned()
        .parse()
        .expect("key");
    let reference = tau_proto::artifact_reference(&key);
    assert_eq!(
        paste_reference(
            &key,
            &tau_cli_term::PasteContent::Png(Arc::from(&b"original"[..]))
        ),
        format!("{reference} (mime_type: image/png)")
    );
    assert_eq!(
        paste_reference(
            &key,
            &tau_cli_term::PasteContent::Text(Arc::from("original"))
        ),
        format!("{reference} (mime_type: text/plain;charset=utf-8)")
    );
}

/// PNG bytes traverse the same upload preflight/chunk/digest flow as text;
/// media bytes do not become provider image messages or a new wire operation.
#[test]
fn png_paste_uses_existing_original_byte_artifact_flow() {
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (_term, handle, _input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    let source: Arc<[u8]> = b"\x89PNG\r\n\x1a\n\x00\xffbinary".as_slice().into();
    let size = source.len();
    control.start_content(
        "s1".parse().expect("session identity"),
        1,
        tau_cli_term::PasteContent::Png(source.clone()),
        handle,
    );
    let available = read_request(&mut reader);
    assert!(matches!(available.op, ArtifactOp::Available));
    reply(&control, available, Ok(ArtifactValue::Done));
    let begin = read_request(&mut reader);
    assert!(
        matches!(begin.op, ArtifactOp::Begin { size: ref actual, .. } if actual.get() == size as u64)
    );
    let upload = "png-upload".parse().expect("upload identity");
    reply(&control, begin, Ok(ArtifactValue::Upload { upload }));
    let write = read_request(&mut reader);
    assert!(
        matches!(write.op, ArtifactOp::Write { offset: 0, ref bytes, .. } if bytes == &*source)
    );
    reply(
        &control,
        write,
        Ok(ArtifactValue::Written {
            next_offset: size as u64,
        }),
    );
    let finalize = read_request(&mut reader);
    assert!(matches!(finalize.op, ArtifactOp::Finalize { .. }));
    let identity = tau_proto::ArtifactIdentity::metadata(None, Some("image/png".into())).unwrap();
    let key = identity.digest(&source).to_hex()[..24]
        .to_owned()
        .parse()
        .expect("original digest key");
    reply(
        &control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            key,
            size: tau_proto::ArtifactSize::new(size as u64).expect("bounded original size"),
            identity,
        })),
    );
    control.stop();
    worker.join().expect("upload worker exit");
}

/// Unrelated/late results do not advance a transfer, and Cancel sends Abort
/// while leaving the interactive writer usable.
#[test]
fn paste_upload_correlates_retries_and_cancel_without_closing_ui() {
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer.clone());
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (_term, handle, _input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    let session = "s1".parse().expect("session");
    let text: Arc<str> = "x".repeat(8192).into();
    control.start(session, 1, text, handle);
    let available = read_request(&mut reader);
    assert!(matches!(available.op, ArtifactOp::Available));
    control.deliver(ArtifactResult {
        request_id: "unrelated".parse().expect("request id"),
        result: Ok(ArtifactValue::Done),
    });
    assert_eq!(
        *control.expected.lock().expect("correlation lock"),
        Some(available.request_id.clone())
    );
    control.deliver(ArtifactResult {
        request_id: available.request_id,
        result: Ok(ArtifactValue::Done),
    });
    let begin = read_request(&mut reader);
    assert!(matches!(begin.op, ArtifactOp::Begin { .. }));
    let upload_id: tau_proto::ArtifactUploadId = "upload-1".parse().expect("upload id");
    control.deliver(ArtifactResult {
        request_id: begin.request_id,
        result: Ok(ArtifactValue::Upload {
            upload: upload_id.clone(),
        }),
    });
    let write = read_request(&mut reader);
    assert!(matches!(write.op, ArtifactOp::Write { offset: 0, .. }));
    control.cancel(1);
    let abort = read_request(&mut reader);
    assert!(matches!(abort.op, ArtifactOp::Abort { upload } if upload == upload_id));
    control.deliver(ArtifactResult {
        request_id: write.request_id,
        result: Ok(ArtifactValue::Written { next_offset: 8192 }),
    });
    send_frame(
        &writer,
        &HarnessInputMessage::GetCurrentSession(tau_proto::GetCurrentSession {
            request_id: "still-live".to_owned(),
        }),
    )
    .expect("ordinary UI request");
    assert!(matches!(
        reader.read_message().expect("ordinary UI frame"),
        Some(HarnessInputMessage::GetCurrentSession(_))
    ));
    control.stop();
    worker.join().expect("worker exit");
}

/// Explicit retry resends the same acknowledged upload identity and chunk;
/// only a verified final descriptor releases a reference and media hint into
/// the draft.
#[test]
fn paste_upload_retry_preserves_identity_and_inserts_only_verified_reference() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tau_cli_term_raw::{Event, RawEvent};
    let (ui, peer) = UnixStream::pair().expect("socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (control, worker) = PasteUpload::spawn(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(peer));
    let (term, handle, input) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term_raw::CursorShape::Bar,
    );
    handle.enable_paste_uploads(8192);
    handle.set_buffer("draft: ".to_owned(), 7);
    let text = "x".repeat(8192);
    input
        .send(RawEvent::Paste(text.clone()))
        .expect("paste input");
    let Event::PasteUpload { id, text: source } = term.get_next_event().expect("upload event")
    else {
        panic!("paste")
    };
    control.start("s1".parse().expect("session"), id, source, handle.clone());
    reply(&control, read_request(&mut reader), Ok(ArtifactValue::Done));
    reply(
        &control,
        read_request(&mut reader),
        Ok(ArtifactValue::Upload {
            upload: "retry-upload".parse().expect("upload id"),
        }),
    );
    let first_write = read_request(&mut reader);
    reply(
        &control,
        first_write.clone(),
        Err(tau_proto::ArtifactError::Busy),
    );
    assert!(matches!(
        term.get_next_event().expect("failure notice"),
        Event::Notice(_)
    ));
    assert_eq!(handle.get_buffer(), "draft: ");
    input
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("retry key");
    let Event::PasteUpload { id, text: source } = term.get_next_event().expect("retry event")
    else {
        panic!("retry")
    };
    control.start("s1".parse().expect("session"), id, source, handle.clone());
    let retry = read_request(&mut reader);
    assert_eq!(first_write.op, retry.op);
    assert_ne!(first_write.request_id, retry.request_id);
    reply(
        &control,
        retry,
        Ok(ArtifactValue::Written {
            next_offset: text.len() as u64,
        }),
    );
    let finalize = read_request(&mut reader);
    assert!(matches!(finalize.op, ArtifactOp::Finalize { .. }));
    let identity =
        tau_proto::ArtifactIdentity::metadata(None, Some("text/plain;charset=utf-8".into()))
            .unwrap();
    let key = identity.digest(text.as_bytes()).to_hex()[..24]
        .to_owned()
        .parse()
        .expect("digest key");
    let reference = tau_proto::artifact_reference(&key);
    reply(
        &control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            key,
            size: tau_proto::ArtifactSize::new(text.len() as u64).expect("object size"),
            identity,
        })),
    );
    assert!(matches!(
        term.get_next_event().expect("completion event"),
        Event::BufferChanged
    ));
    assert_eq!(
        handle.get_buffer(),
        format!("draft: {reference} (mime_type: text/plain;charset=utf-8)")
    );
    control.stop();
    worker.join().expect("worker exit");
}

/// Correlates a fake harness reply without bypassing the production
/// demultiplexer.
fn reply(
    control: &PasteUpload,
    request: ArtifactRequest,
    result: Result<ArtifactValue, tau_proto::ArtifactError>,
) {
    control.deliver(ArtifactResult {
        request_id: request.request_id,
        result,
    });
}

/// Reads one artifact request from the same writer used for ordinary UI frames.
fn read_request(
    reader: &mut tau_proto::HarnessInputReader<BufReader<UnixStream>>,
) -> ArtifactRequest {
    let Some(HarnessInputMessage::ArtifactRequest(request)) =
        reader.read_message().expect("artifact request")
    else {
        panic!("expected artifact request")
    };
    request
}
