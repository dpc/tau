use super::*;

#[test]
fn extension_reader_waits_for_initialized_ack() {
    let (reader_stream, writer_stream) = UnixStream::pair().expect("stream pair");
    let (tx, rx) = mpsc::channel();
    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized("conn-test".into(), reader_stream, tx, initialized_rx);

    let mut writer = tau_proto::HarnessInputWriter::new(BufWriter::new(writer_stream));
    writer
        .write_message(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "test-extension".into(),
            client_kind: tau_proto::ClientKind::Tool,
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
        } => {
            assert_eq!(connection_id.as_str(), "conn-test");
            assert!(matches!(
                message.as_ref(),
                tau_proto::HarnessInputMessage::Hello(_)
            ));
        }
        HarnessEvent::Disconnected { .. }
        | HarnessEvent::NewClient(_)
        | HarnessEvent::Command(_) => panic!("unexpected harness event"),
    }
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
    let tx = spawn_writer_thread(FailingWriter, WriterShutdown::KillChild(child), None);

    tx.send(WriterCommand::Message(
        tau_proto::HarnessOutputMessage::Disconnect(tau_proto::Disconnect { reason: None }),
    ))
    .expect("queue output");
    drop(tx);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !process_exists(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    panic!("supervised child was not reaped after writer failure");
}

/// The writer thread should count only output frames it successfully encodes
/// and flushes, so per-extension protocol stats reflect delivered harness
/// traffic instead of merely queued frames.
#[test]
fn writer_records_protocol_io_after_successful_flush() {
    let (reader_stream, writer_stream) = UnixStream::pair().expect("stream pair");
    let meter = tau_client::ProtocolIoMeter::default();
    let tx = spawn_writer_thread(
        writer_stream,
        WriterShutdown::CloseStream,
        Some(meter.clone()),
    );
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
