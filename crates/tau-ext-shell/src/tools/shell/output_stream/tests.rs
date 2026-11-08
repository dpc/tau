use std::sync::mpsc;

use super::*;

fn capture() -> (OutputStreamCapture, mpsc::Receiver<HarnessInputMessage>) {
    let (tx, rx) = mpsc::channel();
    (
        OutputStreamCapture::new(
            Output::channel(tx),
            "call".into(),
            tau_proto::ToolName::new("shell"),
        )
        .expect("stream capture"),
        rx,
    )
}

fn receive(rx: &mpsc::Receiver<HarnessInputMessage>) -> ToolOutputReport {
    match rx.try_recv().expect("due output report") {
        HarnessInputMessage::Emit(emit) => match *emit.event {
            Event::ToolOutputReported(report) => report,
            other => panic!("unexpected event: {other:?}"),
        },
        other => panic!("unexpected output: {other:?}"),
    }
}

/// Only the terminal label's framing LF is removed; source blank lines and live
/// batch framing remain intact, and canonical headers supply one separator.
#[test]
fn terminal_tail_has_no_extra_separator_and_preserves_source_blank_lines() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    capture.push(OutputStream::Stdout, b"live\n", start);
    capture.poll(start + Duration::from_secs(15));
    assert_eq!(receive(&rx).text, "\nout live");
    capture.push(
        OutputStream::Stdout,
        b"\nxyz\n\n",
        start + Duration::from_secs(16),
    );
    let tail = capture.finish();
    assert_eq!(tail, "out \nout xyz\nout ");
    let response = tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Map(vec![
        (
            tau_proto::CborValue::Text("duration_seconds".into()),
            tau_proto::CborValue::Text("1".into()),
        ),
        (
            tau_proto::CborValue::Text("output".into()),
            tau_proto::CborValue::Text(tail),
        ),
    ]));
    assert_eq!(
        response.render(),
        "duration_seconds: 1\n\nout \nout xyz\nout "
    );
    assert_eq!(capture.finish(), "");
    assert!(rx.try_recv().is_err());
}

/// Activity resets the idle deadline but cannot extend a batch past 30s.
/// Polling a silent stream never emits an empty heartbeat.
#[test]
fn activity_and_absolute_deadlines_bound_batches_without_early_size_flush() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    capture.poll(start + Duration::from_secs(100));
    assert!(capture.deadline().is_none());
    assert!(rx.try_recv().is_err());
    capture.push(OutputStream::Stdout, b"first", start);
    capture.poll(start + Duration::from_secs(14));
    assert!(rx.try_recv().is_err());
    capture.push(
        OutputStream::Stderr,
        b"second",
        start + Duration::from_secs(14),
    );
    assert_eq!(capture.deadline(), Some(start + Duration::from_secs(29)));
    capture.push(
        OutputStream::Stdout,
        &vec![b'x'; 32 * 1024],
        start + Duration::from_secs(28),
    );
    capture.poll(start + Duration::from_secs(29));
    assert!(rx.try_recv().is_err());
    assert_eq!(capture.deadline(), Some(start + Duration::from_secs(30)));
    capture.poll(start + Duration::from_secs(30));
    let report = receive(&rx);
    assert_eq!(report.sequence, 0);
    assert!(report.text.starts_with("\nout(no_nl) "));
    assert!(report.text.len() <= tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    assert!(report.omitted_bytes > 0);
    capture.poll(start + Duration::from_secs(300));
    assert!(rx.try_recv().is_err());
    capture.push(
        OutputStream::Stderr,
        b"last",
        start + Duration::from_secs(300),
    );
    capture.poll(start + Duration::from_secs(315));
    let report = receive(&rx);
    assert_eq!(report.sequence, 1);
    assert_eq!(report.text, "\nerr(no_nl) last");
    assert_eq!(report.omitted_bytes, 0);
}

/// UTF-8 carries are independent for each pipe and partial codepoints do
/// not become empty updates. The terminal returns only the unsent tail.
#[test]
fn split_codepoints_and_terminal_tail_preserve_stream_identity() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    capture.push(OutputStream::Stdout, &[0xe2, 0x82], start);
    capture.poll(start + Duration::from_secs(15));
    assert!(rx.try_recv().is_err());
    capture.push(
        OutputStream::Stderr,
        b"warning",
        start + Duration::from_secs(16),
    );
    capture.push(
        OutputStream::Stdout,
        &[0xac],
        start + Duration::from_secs(17),
    );
    capture.poll(start + Duration::from_secs(32));
    let report = receive(&rx);
    assert_eq!(report.text, "\nerr(no_nl) warning\nout(no_nl) €");
    capture.push(
        OutputStream::Stderr,
        &[0xff, 0xe2],
        start + Duration::from_secs(33),
    );
    assert_eq!(capture.finish(), "err(no_nl) ��");
    assert!(rx.try_recv().is_err());
}

/// Filling the lifetime update budget stops publishing, not draining or
/// retaining recent diagnostics; the reserved terminal still has a tail.
#[test]
fn lifetime_budget_preserves_recent_terminal_diagnostics() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    let noisy = vec![b'x'; 32 * 1024];
    for batch in 0..8 {
        let now = start + Duration::from_secs(batch * 31);
        capture.push(OutputStream::Stdout, &noisy, now);
        capture.poll(now + Duration::from_secs(15));
        let report = receive(&rx);
        assert_eq!(report.sequence, batch);
        assert_eq!(report.text.len(), tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    }
    assert!(capture.deadline().is_none());
    capture.push(
        OutputStream::Stdout,
        &noisy,
        start + Duration::from_secs(300),
    );
    capture.push(
        OutputStream::Stderr,
        b"fatal: recent diagnostic",
        start + Duration::from_secs(301),
    );
    capture.poll(start + Duration::from_secs(400));
    assert!(rx.try_recv().is_err());
    let terminal = capture.finish();
    assert!(terminal.contains("rendered bytes omitted before retained terminal tail"));
    assert!(terminal.ends_with("\nerr(no_nl) fatal: recent diagnostic"));
    assert!(!terminal.contains("err(no_nl) x"));
}
#[cfg(target_os = "linux")]
mod pressure;

/// Segments preserve source endings rather than marking entire multiline runs
/// as incomplete. A no-newline segment can continue after a batch or pipe
/// switch.
#[test]
fn line_segments_reuse_endings_across_batches_and_pipes() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    capture.push(
        OutputStream::Stdout,
        b"first\nsecond\r\nthird\rfourth",
        start,
    );
    capture.poll(start + Duration::from_secs(15));
    assert_eq!(
        receive(&rx).text,
        "\nout first\nout(crlf) second\nout(cr) third\nout(no_nl) fourth"
    );
    capture.push(
        OutputStream::Stderr,
        b"warning\n",
        start + Duration::from_secs(16),
    );
    capture.push(
        OutputStream::Stdout,
        b" continued\nlast",
        start + Duration::from_secs(17),
    );
    capture.poll(start + Duration::from_secs(32));
    assert_eq!(
        receive(&rx).text,
        "\nerr warning\nout  continued\nout(no_nl) last"
    );
    assert_eq!(capture.finish(), "");
}

/// A CR split from its LF needs only per-pipe boundary state, not a buffered
/// unbounded line. Even a CR published after a partial segment stays explicit.
#[test]
fn split_crlf_and_final_cr_preserve_pipe_endings() {
    let (mut capture, rx) = capture();
    let start = Instant::now();
    capture.push(OutputStream::Stdout, b"first\r", start);
    capture.push(OutputStream::Stderr, b"warning\r", start);
    capture.poll(start + Duration::from_secs(15));
    assert_eq!(receive(&rx).text, "\nout(no_nl) first\nerr(no_nl) warning");
    capture.push(
        OutputStream::Stdout,
        b"\nsecond\r",
        start + Duration::from_secs(16),
    );
    capture.push(OutputStream::Stderr, b"\n", start + Duration::from_secs(17));
    capture.push(
        OutputStream::Stdout,
        b"third\r",
        start + Duration::from_secs(18),
    );
    capture.poll(start + Duration::from_secs(33));
    assert_eq!(
        receive(&rx).text,
        "\nout(crlf) \nout(no_nl) second\nerr(crlf) \nout(cr) \nout(no_nl) third"
    );
    assert_eq!(capture.finish(), "out(cr) ");
}

/// A long unterminated line stays bounded and does not suppress later complete
/// lines; omission counts cover exactly the normalized rendered bytes dropped.
#[test]
fn long_line_and_multiline_omissions_retain_labeled_recent_tail() {
    let mut pending = PendingOutput::default();
    let long = "x".repeat(2 * tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    pending.push(false, &long);
    let (text, omitted) = pending.take(tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    assert_eq!(text.len(), tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    assert_eq!(
        text.len() as u64 + omitted,
        (long.len() + "\nout(no_nl) ".len()) as u64
    );
    pending.push(false, &"line\n".repeat(4000));
    let (text, omitted) = pending.take(tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    assert!(text.starts_with("\nout "));
    assert!(!text.contains("no_nl"));
    assert_eq!(
        text.len() as u64 + omitted,
        ("\nout line".len() * 4000) as u64
    );
}
