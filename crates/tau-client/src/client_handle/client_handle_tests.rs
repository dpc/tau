use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::*;

/// Thread-safe byte sink used to inspect frames emitted by the writer.
#[derive(Clone, Default)]
struct TestWriter {
    /// Encoded protocol frames written by the writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for TestWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("lock test writer")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Construct one declaration whose startup-buffer treatment is easy to
/// distinguish from ordinary output.
fn declaration() -> tau_proto::HarnessInputMessage {
    tau_proto::HarnessInputMessage::Subscribe(tau_proto::Subscribe {
        historical_selectors: Vec::new(),
        live_selectors: Vec::new(),
    })
}

/// Decode every frame captured by the test writer.
fn frames(writer: &TestWriter) -> Vec<tau_proto::HarnessInputMessage> {
    let bytes = writer.bytes.lock().expect("lock test writer").clone();
    let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode test frame") {
        frames.push(frame);
    }
    frames
}

/// Start one handle and writer thread for direct startup-admission tests.
fn handle_and_writer() -> (
    ClientHandle,
    TestWriter,
    std::thread::JoinHandle<ClientResult<()>>,
) {
    let writer = TestWriter::default();
    let captured = writer.clone();
    let (sender, receiver) = crate::writer_thread::writer_channel();
    let handle = ClientHandle::new(sender);
    let writer_thread =
        std::thread::spawn(move || crate::writer_thread::run_writer(writer, receiver));
    (handle, captured, writer_thread)
}

/// A declaration admitted before Configure closure remains in the finite
/// buffer and drains between static startup output and `Ready`.
#[test]
fn declaration_admitted_before_configure_close_reaches_startup_drain() {
    let (handle, writer, writer_thread) = handle_and_writer();
    handle.set_configuring(true);
    handle
        .send(declaration())
        .expect("admit declaration before close");
    handle.set_configuring(false);

    handle
        .flush_configure_outputs()
        .expect("drain admitted declaration");
    handle.send_ready(None).expect("send Ready");
    handle.shutdown().expect("shutdown writer");
    writer_thread.join().expect("join writer").expect("writer");

    assert!(matches!(
        frames(&writer).as_slice(),
        [
            tau_proto::HarnessInputMessage::Subscribe(_),
            tau_proto::HarnessInputMessage::Ready(_)
        ]
    ));
}

/// Configure closure holding the admission mutex wins over a concurrent
/// declaration and prevents a successful orphan from entering the buffer.
#[test]
fn configure_close_before_declaration_admission_returns_unavailable() {
    let (handle, _writer, writer_thread) = handle_and_writer();
    handle.set_configuring(true);
    let mut configure_outputs = handle
        .configure_outputs
        .lock()
        .expect("lock Configure admission");
    let sender_handle = handle.clone();
    let (started_sender, started_receiver) = mpsc::channel();
    let sender = std::thread::spawn(move || {
        started_sender.send(()).expect("signal sender start");
        sender_handle.send(declaration())
    });
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("sender started");
    configure_outputs.accepting = false;
    drop(configure_outputs);

    let error = sender
        .join()
        .expect("join declaration sender")
        .expect_err("closed admission must reject pre-Ready declaration");
    assert_eq!(
        error.to_string(),
        "client output is unavailable before startup Ready"
    );
    assert!(
        handle
            .configure_outputs
            .lock()
            .expect("lock Configure buffer")
            .pending
            .is_empty()
    );

    handle.shutdown().expect("shutdown writer");
    writer_thread.join().expect("join writer").expect("writer");
}

/// Rejection discards every admitted declaration, and a declaration that
/// loses the closure race cannot repopulate the discarded startup buffer.
#[test]
fn configure_rejection_discards_admitted_output_without_late_repopulation() {
    let (handle, _writer, writer_thread) = handle_and_writer();
    handle.set_configuring(true);
    handle
        .send(declaration())
        .expect("admit declaration before rejection");
    handle.set_configuring(false);
    handle.discard_configure_outputs();

    handle
        .send(declaration())
        .expect_err("closed pre-Ready admission must reject declaration");
    assert!(
        handle
            .configure_outputs
            .lock()
            .expect("lock Configure buffer")
            .pending
            .is_empty()
    );

    handle.shutdown().expect("shutdown writer");
    writer_thread.join().expect("join writer").expect("writer");
}

/// Closed Configure admission preserves the ordinary controls: declarations
/// fail before `Ready` and use immediate writer output after `Ready`.
#[test]
fn closed_configure_admission_preserves_pre_and_post_ready_controls() {
    let (handle, writer, writer_thread) = handle_and_writer();
    handle
        .send(declaration())
        .expect_err("pre-Ready declaration must be unavailable");
    handle.send_ready(None).expect("send Ready");
    handle
        .send(declaration())
        .expect("post-Ready declaration uses immediate output");
    handle.shutdown().expect("shutdown writer");
    writer_thread.join().expect("join writer").expect("writer");

    assert!(matches!(
        frames(&writer).as_slice(),
        [
            tau_proto::HarnessInputMessage::Ready(_),
            tau_proto::HarnessInputMessage::Subscribe(_)
        ]
    ));
}
