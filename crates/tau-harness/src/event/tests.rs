use std::process::Stdio;

use super::*;

#[test]
fn extension_reader_waits_for_initialized_ack() {
    let (reader_stream, writer_stream) = UnixStream::pair().expect("stream pair");
    let (tx, rx) = mpsc::channel();
    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        crate::test_connection_id("conn-test"),
        reader_stream,
        tx,
        initialized_rx,
    );

    let mut writer = tau_proto::HarnessInputWriter::new(BufWriter::new(writer_stream));
    writer
        .write_message(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("test-extension"),
            client_kind: tau_proto::ClientKind::Tool,
            capabilities: Default::default(),
        }))
        .expect("write hello");
    writer.flush().expect("flush hello");

    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    initialized_tx.send(()).expect("send initialized ack");
    let event = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reader forwards after initialized ack");
    match event {
        HarnessEvent::FromConnection {
            connection_id,
            message,
            frame_bytes: _,
        } => {
            assert_eq!(connection_id.as_str(), "conn-test");
            assert!(matches!(
                message.as_ref(),
                tau_proto::HarnessInputMessage::Hello(_)
            ));
        }
        HarnessEvent::Disconnected { .. }
        | HarnessEvent::ReadFailed { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::SupervisedWriterCleanupComplete { .. }
        | HarnessEvent::Command(_) => panic!("unexpected harness event"),
    }
}

#[test]
fn reader_reports_decode_failure_separately_from_clean_disconnect() {
    let (reader_stream, mut writer_stream) = UnixStream::pair().expect("stream pair");
    let (tx, rx) = mpsc::channel();
    spawn_reader_thread(
        crate::test_connection_id("conn-malformed"),
        reader_stream,
        tx,
    );

    writer_stream
        .write_all(&[0xff])
        .expect("write malformed cbor");
    writer_stream
        .shutdown(std::net::Shutdown::Write)
        .expect("eof");

    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ReadFailed { connection_id, .. })
            if connection_id.as_str() == "conn-malformed"
    ));
}

/// Test writer that fails every write to exercise supervised-child cleanup.
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test writer failed",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists and is
    // signalable; it does not deliver a signal.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
    }
}

/// Ensures supervised extension writer failures still run child cleanup so
/// broken pipes do not leave duplicate extension processes behind.
#[test]
fn writer_failure_still_reaps_supervised_child() {
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 30")
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    let (harness_tx, harness_rx) = mpsc::channel();
    let (tx, mut writer) = spawn_supervised_writer_thread(
        crate::test_connection_id("failing-writer"),
        FailingWriter,
        child,
        None,
        harness_tx,
    );

    tx.send(WriterCommand::Message(
        tau_proto::HarnessOutputMessage::Disconnect(tau_proto::Disconnect { reason: None }),
    ))
    .expect("queue output");
    drop(tx);
    assert!(matches!(
        harness_rx.recv_timeout(Duration::from_secs(5)),
        Ok(HarnessEvent::SupervisedWriterCleanupComplete { connection_id })
            if connection_id.as_str() == "failing-writer"
    ));
    writer.join().expect("join failing writer");
    assert!(!process_exists(pid));
}

/// A child that consumes stdin and exits on EOF should complete before the
/// watchdog signals it.
#[test]
fn graceful_supervised_writer_cleanup_cancels_watchdog() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let marker = tempdir.path().join("graceful");
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("cat >/dev/null; printf graceful > \"$1\"")
        .arg("sh")
        .arg(&marker)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let stdin = child.stdin.take().expect("child stdin");
    let (harness_tx, harness_rx) = mpsc::channel();
    let (writer_tx, mut writer) = spawn_supervised_writer_thread(
        crate::test_connection_id("graceful-writer"),
        stdin,
        child,
        None,
        harness_tx,
    );

    drop(writer_tx);
    let watchdog = writer.start_shutdown_watchdog();
    writer.join().expect("join writer");
    watchdog.join().expect("join watchdog");

    assert!(!writer.watchdog_fired());
    assert_eq!(
        std::fs::read_to_string(marker).expect("graceful marker"),
        "graceful"
    );
    assert!(matches!(
        harness_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::SupervisedWriterCleanupComplete { connection_id })
            if connection_id.as_str() == "graceful-writer"
    ));
}

/// Reaching child wait after the outer deadline must not start a second grace
/// period.
#[test]
fn expired_cleanup_deadline_is_carried_into_child_wait() {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("exec sleep 30")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    let stdin = child.stdin.take().expect("child stdin");
    let (harness_tx, harness_rx) = mpsc::channel();
    let (writer_tx, mut writer) = spawn_supervised_writer_thread(
        crate::test_connection_id("expired-writer"),
        stdin,
        child,
        None,
        harness_tx,
    );
    writer.arm_cleanup_deadline(Instant::now());
    let started = Instant::now();

    drop(writer_tx);
    assert!(matches!(
        harness_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::SupervisedWriterCleanupComplete { connection_id })
            if connection_id.as_str() == "expired-writer"
    ));
    writer.join().expect("join expired writer");

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!process_exists(pid));
}

/// A shutdown watcher must preserve an earlier runtime deadline rather than
/// granting a blocked writer a fresh grace period.
#[test]
fn shutdown_watchdog_uses_prearmed_runtime_deadline() {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("exec sleep 30")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    let stdin = child.stdin.take().expect("child stdin");
    let (harness_tx, harness_rx) = mpsc::channel();
    let (writer_tx, mut writer) = spawn_supervised_writer_thread(
        crate::test_connection_id("prearmed-writer"),
        stdin,
        child,
        None,
        harness_tx,
    );
    writer.arm_cleanup_deadline(Instant::now() + Duration::from_millis(200));
    writer_tx
        .send(WriterCommand::Message(HarnessOutputMessage::deliver(
            tau_proto::Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                message: "x".repeat(2 * 1024 * 1024),
                level: tau_proto::NoticeLevel::Info,
                always_show: false,
            }),
        )))
        .expect("queue blocking frame");
    let started = Instant::now();

    let watchdog = writer.start_shutdown_watchdog();
    drop(writer_tx);
    assert!(matches!(
        harness_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::SupervisedWriterCleanupComplete { connection_id })
            if connection_id.as_str() == "prearmed-writer"
    ));
    writer.join().expect("join prearmed writer");
    watchdog.join().expect("join prearmed watchdog");

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!process_exists(pid));
}

/// The writer thread should count only output frames it successfully encodes
/// and flushes, so per-extension protocol stats reflect delivered harness
/// traffic instead of merely queued frames.
#[test]
fn writer_records_protocol_io_after_successful_flush() {
    let (reader_stream, writer_stream) = UnixStream::pair().expect("stream pair");
    let meter = tau_client::ProtocolIoMeter::default();
    let tx = spawn_writer_thread(writer_stream, Some(meter.clone()));
    tx.send(WriterCommand::Message(
        tau_proto::HarnessOutputMessage::deliver(tau_proto::Event::TermBell(
            tau_proto::TermBell {},
        )),
    ))
    .expect("queue output");

    let mut reader = tau_proto::HarnessOutputReader::new(BufReader::new(reader_stream));
    let _ = reader.read_message().expect("read output");

    let stats = meter.cumulative_stats();
    let event_stats = stats.downlink.get("term.bell").expect("term bell stats");
    assert_eq!(event_stats.count, 1);
    assert!(0 < event_stats.bytes);
}
