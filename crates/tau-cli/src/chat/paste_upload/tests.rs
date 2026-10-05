use std::io::BufReader;
use std::os::unix::net::UnixStream;

use super::*;
use crate::chat::{UiIoMeter, UiWriter};

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
    let key = format!("blake3:{}", blake3::hash(b"original").to_hex())
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
        matches!(begin.op, ArtifactOp::Begin { size: ref actual } if actual.get() == size as u64)
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
    let key = format!("blake3:{}", blake3::hash(&source).to_hex())
        .parse()
        .expect("original digest key");
    reply(
        &control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            key,
            size: tau_proto::ArtifactSize::new(size as u64).expect("bounded original size"),
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
    let key = format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
        .parse()
        .expect("digest key");
    let reference = tau_proto::artifact_reference(&key);
    reply(
        &control,
        finalize,
        Ok(ArtifactValue::Descriptor(tau_proto::ArtifactDescriptor {
            key,
            size: tau_proto::ArtifactSize::new(text.len() as u64).expect("object size"),
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
