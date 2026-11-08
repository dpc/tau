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
    assert!(report.text.starts_with("\nout (fragment) "));
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
    assert_eq!(report.text, "\nerr (fragment) last");
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
    assert_eq!(report.text, "\nerr (fragment) warning\nout (fragment) €");
    capture.push(
        OutputStream::Stderr,
        &[0xff, 0xe2],
        start + Duration::from_secs(33),
    );
    assert_eq!(capture.finish(), "\nerr (fragment) ��");
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
    assert!(terminal.ends_with("\nerr (fragment) fatal: recent diagnostic"));
    assert!(!terminal.contains("err (fragment) x"));
}
#[cfg(target_os = "linux")]
mod pressure;
