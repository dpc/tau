use std::process::Stdio;
use std::{net as path_std_net, process as path_std_process};

use tau_proto::{Event, HarnessOutputReader};

use super::*;
use crate::event_log::EventLog;

fn disconnected_event_named(name: &str) -> HarnessEvent {
    HarnessEvent::Disconnected {
        connection_id: crate::test_connection_id(name),
    }
}

fn disconnected_event() -> HarnessEvent {
    disconnected_event_named("ingress-test")
}

/// Capacity zero must rendezvous with harness consumption rather than using
/// timing or a hidden forwarding queue as correctness authority.
#[test]
fn component_ingress_capacity_zero_rendezvous() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ingress, sender) = ComponentIngress::new(wake_tx, ComponentIngressCapacity::Rendezvous);
    let (done_tx, done_rx) = mpsc::channel();
    let producer = thread::spawn(move || {
        sender.send(disconnected_event()).expect("send ingress");
        done_tx.send(()).expect("report completion");
    });

    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(
        ingress.take_ready(),
        Some(HarnessEvent::Disconnected { .. })
    ));
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("rendezvous completes after consumption");
    producer.join().expect("producer joins");
}

/// Rendezvous completion must acknowledge the sender's own consumed payload,
/// even when a blocked successor immediately occupies the shared slot.
#[test]
fn component_ingress_rendezvous_acknowledges_own_ticket() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ingress, sender) = ComponentIngress::new(wake_tx, ComponentIngressCapacity::Rendezvous);
    let (first_done_tx, first_done_rx) = mpsc::channel();
    let first_sender = sender.clone();
    let first = thread::spawn(move || {
        first_done_tx
            .send(first_sender.send(disconnected_event_named("first-rendezvous")))
            .expect("report first");
    });
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));

    let second_sender = sender.clone();
    let second =
        thread::spawn(move || second_sender.send(disconnected_event_named("second-rendezvous")));
    ingress.wait_for_blocked_sender();
    let Some(HarnessEvent::Disconnected { connection_id }) = ingress.take_ready() else {
        panic!("first rendezvous payload");
    };
    assert_eq!(connection_id.as_str(), "first-rendezvous");
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    assert_eq!(
        first_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first sender acknowledges its own payload"),
        Ok(())
    );
    ingress.close();
    assert_eq!(second.join().expect("second producer joins"), Err(()));
    first.join().expect("first producer joins");
}

/// Capacity one must block a second producer behind the occupied slot, retain
/// both payloads in order, and release a saturated producer on shutdown.
#[test]
fn component_ingress_capacity_one_saturates_orders_and_closes() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ingress, sender) = ComponentIngress::new(wake_tx, ComponentIngressCapacity::One);
    sender
        .send(disconnected_event_named("first"))
        .expect("send first");
    let (done_tx, done_rx) = mpsc::channel();
    let second_sender = sender.clone();
    let second = thread::spawn(move || {
        let result = second_sender.send(disconnected_event_named("second"));
        done_tx.send(result).expect("report second result");
    });
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    ingress.wait_for_blocked_sender();
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let Some(HarnessEvent::Disconnected { connection_id }) = ingress.take_ready() else {
        panic!("first payload");
    };
    assert_eq!(connection_id.as_str(), "first");
    assert_eq!(
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second enters after first consumption"),
        Ok(())
    );
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    let Some(HarnessEvent::Disconnected { connection_id }) = ingress.take_ready() else {
        panic!("second payload");
    };
    assert_eq!(connection_id.as_str(), "second");
    second.join().expect("second producer joins");

    sender
        .send(disconnected_event_named("occupied"))
        .expect("occupy slot");
    let blocked_sender = sender.clone();
    let blocked = thread::spawn(move || blocked_sender.send(disconnected_event_named("blocked")));
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    ingress.wait_for_blocked_sender();
    ingress.close();
    assert_eq!(blocked.join().expect("blocked producer joins"), Err(()));
}

/// Ensures a Ready frame keeps its exact decode observation while its reader
/// blocks behind the occupied ingress slot implicated in startup deadline
/// races.
#[test]
fn blocked_ready_preserves_decode_observation_until_ingress_consumption() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ingress, sender) = ComponentIngress::new(wake_tx, ComponentIngressCapacity::One);
    sender
        .send(disconnected_event_named("occupied"))
        .expect("occupy ingress");
    let ready_connection = crate::test_connection_id("blocked-ready");
    let decoded_at = Instant::now();
    let blocked_sender = sender.clone();
    let blocked_connection = ready_connection.clone();
    let producer = thread::spawn(move || {
        blocked_sender.send(HarnessEvent::from_connection_observed_at_for_test(
            blocked_connection,
            tau_proto::HarnessInputMessage::Ready(Default::default()),
            decoded_at,
        ))
    });

    ingress.wait_for_blocked_sender();
    assert!(
        ingress
            .startup_frame_observations()
            .iter()
            .any(|observation| {
                observation.connection_id == ready_connection
                    && observation.kind == StartupFrameKind::Ready
                    && observation.decoded_at == decoded_at
            })
    );
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    let _occupied = ingress.take_ready().expect("consume occupied frame");
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    let ready = ingress.take_ready().expect("consume blocked Ready");
    assert!(matches!(
        ready,
        HarnessEvent::FromConnection {
            connection_id,
            message,
            decoded_at: observed_at,
            ..
        } if connection_id == ready_connection
            && matches!(message.as_ref(), tau_proto::HarnessInputMessage::Ready(_))
            && observed_at == decoded_at
    ));
    assert_eq!(producer.join().expect("producer joins"), Ok(()));
}

/// Closing component ingress during shutdown must wake a rendezvous producer
/// so joining a reader cannot deadlock after the event loop stops.
#[test]
fn component_ingress_close_wakes_blocked_sender() {
    let (wake_tx, wake_rx) = mpsc::channel();
    let (ingress, sender) = ComponentIngress::new(wake_tx, ComponentIngressCapacity::Rendezvous);
    let producer = thread::spawn(move || sender.send(disconnected_event()));
    assert!(matches!(
        wake_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    ingress.close();
    assert_eq!(producer.join().expect("producer joins"), Err(()));
}

#[test]
fn extension_reader_waits_for_initialized_ack() {
    let (reader_stream, writer_stream) = UnixStream::pair().expect("stream pair");
    let (tx, rx) = mpsc::channel();
    let (ingress, ingress_tx) = ComponentIngress::new(tx, ComponentIngressCapacity::One);
    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        crate::test_connection_id("conn-test"),
        reader_stream,
        ingress_tx,
        initialized_rx,
    );

    let mut writer = tau_proto::HarnessInputWriter::new(BufWriter::new(writer_stream));
    writer
        .write_message(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("test-extension"),
            client_kind: tau_proto::ClientKind::Tool,
            expected_session_id: None,
            capabilities: Default::default(),
        }))
        .expect("write hello");
    writer.flush().expect("flush hello");

    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    initialized_tx.send(()).expect("send initialized ack");
    let wake = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reader forwards after initialized ack");
    assert!(matches!(wake, HarnessEvent::ComponentIngressReady));
    let event = ingress.take_ready().expect("ingress payload");
    match event {
        HarnessEvent::FromConnection {
            connection_id,
            message,
            frame_bytes: _,
            decoded_at: _,
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
        | HarnessEvent::Command(_)
        | HarnessEvent::ComponentIngressReady => panic!("unexpected harness event"),
    }
}

#[test]
fn reader_reports_decode_failure_separately_from_clean_disconnect() {
    let (reader_stream, mut writer_stream) = UnixStream::pair().expect("stream pair");
    let (tx, rx) = mpsc::channel();
    let (ingress, ingress_tx) = ComponentIngress::new(tx, ComponentIngressCapacity::One);
    spawn_reader_thread(
        crate::test_connection_id("conn-malformed"),
        reader_stream,
        ingress_tx,
    );

    writer_stream
        .write_all(&[0xff])
        .expect("write malformed cbor");
    writer_stream
        .shutdown(path_std_net::Shutdown::Write)
        .expect("eof");

    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ComponentIngressReady)
    ));
    assert!(matches!(
        ingress.take_ready(),
        Some(HarnessEvent::ReadFailed { connection_id, .. })
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

/// Thread-safe byte collector used to observe follower transport output.
#[derive(Clone, Default)]
struct SharedWriter(
    /// Bytes flushed by a production follower under test.
    Arc<Mutex<Vec<u8>>>,
);

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("shared writer")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decoded_output_count(writer: &SharedWriter) -> usize {
    let bytes = writer.0.lock().expect("shared writer").clone();
    let mut reader = HarnessOutputReader::new(BufReader::new(bytes.as_slice()));
    let mut count = 0;
    while reader.read_message().expect("decode output").is_some() {
        count += 1;
    }
    count
}

/// An initial-stdio UI follower failure must retire its cursor without
/// overtaking the independent ingress reader's ordered disconnect.
#[test]
fn initial_stdio_follower_failure_waits_for_ingress_disconnect() {
    let log = EventLog::new();
    let (control_tx, control_rx) = mpsc::channel();
    let writer_tx = spawn_initial_stdio_writer_thread(FailingWriter, None);
    let connection_id = crate::test_connection_id("writer-failure");
    let mut sink = ChannelSink::new(
        &writer_tx,
        Arc::clone(&log),
        control_tx,
        connection_id.clone(),
    )
    .expect("configure follower");
    let consumer = sink.handle();
    sink.send(tau_core::RoutedFrame::new(
        None,
        HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "test".to_owned(),
            message: "fail".to_owned(),
            level: tau_proto::NoticeLevel::Info,
            purpose: tau_proto::NoticePurpose::Diagnostic,
        })),
    ))
    .expect("admit frame");

    assert!(
        consumer.wait_for_retirement(Duration::from_secs(1)),
        "failing initial-stdio writer did not retire its consumer"
    );
    assert!(
        matches!(
            control_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
        ),
        "downlink failure must not overtake ingress detach/disconnect ordering"
    );
}

/// Failing follower setup must roll back its registered generation so a closed
/// writer command receiver cannot pin later shared retention.
#[test]
fn shared_follower_setup_failure_retires_generation() {
    let log = EventLog::new();
    let (writer_tx, writer_rx) = mpsc::channel();
    drop(writer_rx);
    let (control_tx, _control_rx) = mpsc::channel();
    let result = ChannelSink::new(
        &writer_tx,
        Arc::clone(&log),
        control_tx,
        crate::test_connection_id("closed-writer"),
    );
    assert!(result.is_err());
    assert_eq!(log.consumer_count(), 0);
}

/// A writer-retired generation that remains briefly registered in EventBus
/// must reject a directed route instead of claiming route admission.
#[test]
fn retired_shared_generation_is_not_reported_as_directed_delivery() {
    let log = EventLog::new();
    let (control_tx, control_rx) = mpsc::channel();
    let writer_tx = spawn_writer_thread(FailingWriter, None);
    let connection_id = crate::test_connection_id("retired-directed");
    let sink = ChannelSink::new(
        &writer_tx,
        Arc::clone(&log),
        control_tx,
        connection_id.clone(),
    )
    .expect("configure follower");
    let mut bus = tau_core::EventBus::new();
    bus.connect(tau_core::Connection::new(
        tau_core::PendingConnectionMetadata {
            id: Some(connection_id.clone()),
            name: crate::test_extension_name("retired-directed"),
            kind: tau_proto::ClientKind::Provider,
            origin: tau_core::ConnectionOrigin::InMemory,
        },
        Box::new(sink),
    ));
    let frame = HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test".to_owned(),
        message: "first".to_owned(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    }));
    let first = bus
        .send_to(&connection_id, None, frame.clone())
        .expect("first route");
    assert_eq!(
        first.delivered_to.as_slice(),
        std::slice::from_ref(&connection_id)
    );
    assert!(matches!(
        control_rx.recv_timeout(Duration::from_secs(1)),
        Ok(HarnessEvent::ReadFailed { .. })
    ));

    let retired = bus
        .send_to(&connection_id, None, frame)
        .expect("retired route report");
    assert!(retired.delivered_to.is_empty());
    assert_eq!(retired.failed_deliveries.len(), 1);
}

/// Production ChannelSink followers must withhold a paused consumer's live
/// suffix, preserve publication-time eligibility across subscription
/// replacement, and let an independent target advance concurrently.
#[test]
fn shared_channel_sink_catch_up_barrier_freezes_targets_independently() {
    let log = EventLog::new();
    let (control_tx, _control_rx) = mpsc::channel();
    let paused_id = crate::test_connection_id("paused");
    let healthy_id = crate::test_connection_id("healthy");
    let paused_writer = SharedWriter::default();
    let healthy_writer = SharedWriter::default();
    let paused_tx = spawn_writer_thread(paused_writer.clone(), None);
    let healthy_tx = spawn_writer_thread(healthy_writer.clone(), None);
    let paused_sink = ChannelSink::new(
        &paused_tx,
        Arc::clone(&log),
        control_tx.clone(),
        paused_id.clone(),
    )
    .expect("paused sink");
    let paused_handle = paused_sink.handle();
    let healthy_sink = ChannelSink::new(
        &healthy_tx,
        Arc::clone(&log),
        control_tx,
        healthy_id.clone(),
    )
    .expect("healthy sink");
    let healthy_handle = healthy_sink.handle();
    let mut bus = tau_core::EventBus::new();
    for (connection_id, sink) in [
        (
            paused_id.clone(),
            Box::new(paused_sink) as Box<dyn ConnectionSink>,
        ),
        (
            healthy_id.clone(),
            Box::new(healthy_sink) as Box<dyn ConnectionSink>,
        ),
    ] {
        bus.connect(tau_core::Connection::new(
            tau_core::PendingConnectionMetadata {
                id: Some(connection_id),
                name: crate::test_extension_name("shared-test"),
                kind: tau_proto::ClientKind::Tool,
                origin: tau_core::ConnectionOrigin::InMemory,
            },
            sink,
        ));
    }
    let selector = tau_proto::EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE);
    for connection_id in [&paused_id, &healthy_id] {
        bus.set_subscriptions(
            connection_id,
            vec![selector.clone()],
            vec![selector.clone()],
        )
        .expect("subscribe");
    }
    bus.begin_catch_up(&paused_id).expect("pause catch-up");
    let notice = |message: &str| {
        HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "test".to_owned(),
            message: message.to_owned(),
            level: tau_proto::NoticeLevel::Info,
            purpose: tau_proto::NoticePurpose::Diagnostic,
        }))
    };
    bus.publish(notice("frozen"));
    bus.set_subscriptions(&paused_id, vec![selector], Vec::new())
        .expect("replace while paused");
    bus.publish(notice("future-only"));

    healthy_handle.flush();
    assert_eq!(decoded_output_count(&healthy_writer), 2);
    assert!(
        paused_writer.0.lock().expect("paused writer").is_empty(),
        "paused follower must withhold its frozen live suffix"
    );
    bus.finish_catch_up(&paused_id).expect("release catch-up");
    paused_handle.flush();
    assert_eq!(
        decoded_output_count(&paused_writer),
        1,
        "subscription replacement cannot retract the already frozen target"
    );
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
    let child = path_std_process::Command::new("sh")
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
    let mut child = path_std_process::Command::new("sh")
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
    let mut child = path_std_process::Command::new("sh")
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
    let mut child = path_std_process::Command::new("sh")
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
                purpose: tau_proto::NoticePurpose::Diagnostic,
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

/// The writer thread must record an output frame only after a successful flush.
///
/// Reading the Unix stream proves that the peer can observe the frame, but it
/// does not synchronize with the writer's subsequent meter mutation. A FIFO
/// flush acknowledgement therefore establishes that the writer processed the
/// preceding post-flush accounting before this test observes cumulative stats.
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
    let (flush_tx, flush_rx) = mpsc::channel();
    tx.send(WriterCommand::Flush(flush_tx))
        .expect("queue accounting barrier");

    let mut reader = tau_proto::HarnessOutputReader::new(BufReader::new(reader_stream));
    let _ = reader.read_message().expect("read output");
    flush_rx
        .recv()
        .expect("writer processes accounting barrier");

    let stats = meter.cumulative_stats();
    let event_stats = stats.downlink.get("term.bell").expect("term bell stats");
    assert_eq!(event_stats.count, 1);
    assert!(0 < event_stats.bytes);
}

/// Prompt traffic diagnostics must classify only real prompt-bearing frames
/// without retaining content.
#[test]
fn prompt_traffic_classes_are_fixed_content_free_and_prompt_only() {
    let canary = "prompt-canary-must-not-appear";
    let session_id = tau_proto::SessionId::parse("s1").expect("session id");
    let submitted =
        HarnessInputMessage::emit(Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            literal: false,
            session_id: session_id.clone(),
            text: canary.to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    let created = HarnessInputMessage::emit(Event::UiCreateAgent(tau_proto::UiCreateAgent {
        request_id: "request".to_owned(),
        literal: false,
        parent_agent: None,
        session_id: session_id.clone(),
        role: "engineer".to_owned(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: Some(canary.to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    }));
    let draft = HarnessInputMessage::emit(Event::UiPromptDraft(tau_proto::UiPromptDraft {
        session_id,
        target_agent_id: None,
        text: Some(canary.to_owned()),
    }));
    let create_without_prompt =
        HarnessInputMessage::emit(Event::UiCreateAgent(tau_proto::UiCreateAgent {
            request_id: "request-without-prompt".to_owned(),
            literal: false,
            parent_agent: None,
            session_id: tau_proto::SessionId::parse("s1").expect("session id"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            ephemeral: false,
        }));

    let submitted_class =
        prompt_traffic_class_for_message(&submitted).expect("submitted prompt class");
    let created_class = prompt_traffic_class_for_message(&created).expect("created prompt class");
    assert_eq!(submitted_class, "ui_prompt_submitted");
    assert_eq!(created_class, "ui_create_agent");
    assert!(!submitted_class.contains(canary));
    assert!(!created_class.contains(canary));
    assert_eq!(prompt_traffic_class_for_message(&draft), None);
    assert_eq!(
        prompt_traffic_class_for_message(&create_without_prompt),
        None
    );
}
