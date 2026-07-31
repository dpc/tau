use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::{atomic as path_std_sync_atomic, mpsc};

use tau_proto::HarnessOutputWriter;

use super::*;

/// Admission retains buffered bytes so an acknowledgement coalesced with a
/// later disconnect is still visible to the long-lived reader.
#[test]
fn ui_session_admission_preserves_coalesced_followup() {
    let (client, server) = UnixStream::pair().expect("socket pair");
    let expected = tau_proto::SessionId::parse("session-1").expect("valid session id");
    let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(server));
    writer
        .write_message(&HarnessOutputMessage::UiSessionAccepted(
            tau_proto::UiSessionAccepted {
                session_id: expected.clone(),
            },
        ))
        .expect("write admission");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("after admission".to_owned()),
        }))
        .expect("write disconnect");
    writer.flush().expect("flush coalesced messages");

    let mut reader =
        await_ui_session_admission(Box::new(client), expected, None, Duration::from_secs(1))
            .expect("admission succeeds");
    let followup = reader
        .read_message()
        .expect("read followup")
        .expect("followup exists");
    assert!(matches!(
        followup,
        HarnessOutputMessage::Disconnect(disconnect)
            if disconnect.reason.as_deref() == Some("after admission")
    ));
}

/// A peer that withholds admission cannot block startup indefinitely; timeout
/// also shuts down an attached socket so the reader thread can exit.
#[test]
fn ui_session_admission_times_out_and_shuts_down_socket() {
    let (client, mut server) = UnixStream::pair().expect("socket pair");
    let shutdown = client.try_clone().expect("clone client");
    let error = match await_ui_session_admission(
        Box::new(client),
        tau_proto::SessionId::parse("session-1").expect("valid session id"),
        Some(&shutdown),
        Duration::from_millis(10),
    ) {
        Ok(_) => panic!("withheld admission must time out"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("timed out"));
    let mut byte = [0_u8; 1];
    assert_eq!(server.read(&mut byte).expect("read shutdown EOF"), 0);
}

/// The byte budget blocks admission at its exact cap and resumes after
/// dequeue releases bytes.
#[test]
fn renderer_byte_budget_blocks_and_releases_at_cap() {
    let budget = Arc::new(RendererByteBudget::new());
    budget.acquire(RENDERER_QUEUE_MAX_BYTES);
    let blocked_budget = budget.clone();
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        blocked_budget.acquire(1);
        admitted_tx.send(()).expect("admission result");
        blocked_budget.release(1);
    });

    assert!(admitted_rx.recv_timeout(Duration::from_millis(10)).is_err());
    assert_eq!(budget.release(RENDERER_QUEUE_MAX_BYTES), 0);
    admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("byte waiter released");
    waiter.join().expect("byte waiter");
}

/// A disconnect frame must consume the same byte and item permits as a
/// delivery, including when it waits behind an exactly saturated budget.
#[test]
fn renderer_disconnect_waits_at_exact_byte_cap_and_releases_all_permits() {
    let budget = Arc::new(RendererByteBudget::new());
    let queued_items = Arc::new(path_std_sync_atomic::AtomicUsize::new(1));
    budget.acquire(RENDERER_QUEUE_MAX_BYTES);
    let blocked_budget = budget.clone();
    let blocked_items = queued_items.clone();
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        blocked_budget.acquire(1);
        blocked_items.fetch_add(1, Ordering::AcqRel);
        admitted_tx.send(()).expect("disconnect admitted");
        blocked_items.fetch_sub(1, Ordering::AcqRel);
        blocked_budget.release(1);
    });

    assert!(admitted_rx.recv_timeout(Duration::from_millis(10)).is_err());
    assert_eq!(budget.release(RENDERER_QUEUE_MAX_BYTES), 0);
    queued_items.fetch_sub(1, Ordering::AcqRel);
    admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("disconnect admission released");
    waiter.join().expect("disconnect waiter");
    assert_eq!(*budget.used.lock().expect(MUTEX_POISONED), 0);
    assert_eq!(queued_items.load(Ordering::Acquire), 0);
}

/// Production renderer scheduling must drain admitted deliveries and their
/// terminal disconnect before local selection and action commands, then
/// continue draining local work after the remote producer closes.
#[test]
fn renderer_scheduler_preserves_remote_prefix_and_disconnect_order() {
    let (remote_tx, remote_rx) = mpsc::sync_channel(4);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(2));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted.clone(), arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: 1,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("remote delivery");
    remote_tx
        .send(RendererCmd::RemoteDisconnect {
            reason: Some("done".to_owned()),
            delivery_id: 2,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("remote disconnect");
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: "worker".to_owned(),
        })
        .expect("local selection");
    local_tx
        .send(RendererCmd::ActionInvoked {
            invocation_id: tau_proto::ActionInvocationId::parse("action-test")
                .expect("test identifier must satisfy its grammar"),
            owner_agent_id: Some("worker".to_owned()),
        })
        .expect("local action");

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        })
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::RemoteDisconnect { delivery_id: 2, .. })
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::SwitchAgent { agent_id }) if agent_id == "worker"
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::ActionInvoked {
            invocation_id,
            owner_agent_id: Some(owner),
        }) if invocation_id.as_ref() == "action-test" && owner == "worker"
    ));
    assert!(!scheduler.remote_closed());
    drop(remote_tx);
}

/// A remote reservation captured by a local watermark must not be
/// overtaken even when its channel send completes after the local send.
#[test]
fn renderer_scheduler_waits_for_reserved_remote_arriving_after_local() {
    let (remote_tx, remote_rx) = mpsc::sync_channel(2);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted.clone(), arbiter.clone());
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: "worker".to_owned(),
        })
        .expect("local selection");
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: 1,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("reserved remote");

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::SwitchAgent { agent_id }) if agent_id == "worker"
    ));
}

/// Local enqueue cannot linearize between the scheduler's local-empty check
/// and its remote dequeue because both operations share the admission
/// arbiter.
#[test]
fn renderer_scheduler_serializes_local_capture_with_remote_dequeue() {
    let (remote_tx, remote_rx) = mpsc::sync_channel(1);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted, arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: 1,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("later remote");
    let (start_tx, start_rx) = mpsc::channel();
    let (attempting_tx, attempting_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let sender = std::thread::spawn(move || {
        start_rx.recv().expect("start local enqueue");
        attempting_tx.send(()).expect("local enqueue attempting");
        local_tx
            .send(RendererCmd::SwitchAgent {
                agent_id: "worker".to_owned(),
            })
            .expect("local enqueue");
        done_tx.send(()).expect("local enqueue done");
    });

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    let mut after_local_check = || {
        start_tx.send(()).expect("release local sender");
        attempting_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("local sender reached arbiter");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(10)).is_err(),
            "local enqueue crossed the scheduler arbitration boundary"
        );
    };
    assert!(matches!(
        scheduler.recv_timeout_after_local_check(Duration::from_secs(1), &mut after_local_check),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        })
    ));
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("local enqueue completes after remote dequeue");
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_secs(1)),
        Ok(RendererCmd::SwitchAgent { agent_id }) if agent_id == "worker"
    ));
    sender.join().expect("local sender");
}

/// An action ownership command must run after its captured older prefix
/// but before a result delivery admitted later.
#[test]
fn renderer_scheduler_places_action_before_later_remote_result() {
    let (remote_tx, remote_rx) = mpsc::sync_channel(4);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted.clone(), arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: 1,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("older remote");
    local_tx
        .send(RendererCmd::ActionInvoked {
            invocation_id: tau_proto::ActionInvocationId::parse("action-test")
                .expect("test identifier must satisfy its grammar"),
            owner_agent_id: Some("worker".to_owned()),
        })
        .expect("local action");
    admitted.store(2, Ordering::Release);
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(2),
            delivery_id: 2,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("later result");

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        })
    ));
    assert!(matches!(next(), Ok(RendererCmd::ActionInvoked { .. })));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 2,
            ..
        })
    ));
}

/// Later remote arrivals must not starve a local command once its finite
/// admission watermark has been drained.
#[test]
fn renderer_scheduler_bounds_local_progress_under_remote_replenishment() {
    let (remote_tx, remote_rx) = mpsc::sync_channel(8);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(2));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted.clone(), arbiter.clone());
    local_tx
        .send(RendererCmd::ClearSelectedAgent)
        .expect("local selection");
    for delivery_id in 1..=4 {
        admitted.store(delivery_id, Ordering::Release);
        remote_tx
            .send(RendererCmd::Remote {
                abandoned_shell_starts: Vec::new(),
                presentation: cold_attach_stager::RendererPresentation::Ordinary,
                event: Box::new(Event::TermBell(tau_proto::TermBell {})),
                recorded_at: UnixMicros::new(delivery_id),
                delivery_id,
                queue_bytes: 1,
                enqueued_at: Instant::now(),
            })
            .expect("remote delivery");
    }

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 2,
            ..
        })
    ));
    assert!(matches!(next(), Ok(RendererCmd::ClearSelectedAgent)));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 3,
            ..
        })
    ));
}

/// A saturated remote byte budget must not prevent a watermarked local
/// selection from reaching the renderer or a cancellation frame from
/// reaching the harness-side reader.
#[test]
fn saturated_remote_admission_keeps_selection_and_cancel_uplink_live() {
    let budget = Arc::new(RendererByteBudget::new());
    budget.acquire(RENDERER_QUEUE_MAX_BYTES);
    let (remote_tx, remote_rx) = mpsc::sync_channel(1);
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (local_tx, local_rx) = LocalRendererSender::channel(admitted.clone(), arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: 1,
            queue_bytes: RENDERER_QUEUE_MAX_BYTES,
            enqueued_at: Instant::now(),
        })
        .expect("fill real remote queue");
    let blocked_budget = budget.clone();
    let blocked_admitted = admitted.clone();
    let blocked_arbiter = arbiter.clone();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        let mut wait_observed = || blocked_tx.send(()).expect("producer blocked");
        blocked_budget.acquire_after_wait_observed(1, &mut wait_observed);
        {
            let _guard = blocked_arbiter.lock().expect(MUTEX_POISONED);
            blocked_admitted.fetch_add(1, Ordering::AcqRel);
        }
        remote_tx
            .send(RendererCmd::Remote {
                abandoned_shell_starts: Vec::new(),
                presentation: cold_attach_stager::RendererPresentation::Ordinary,
                event: Box::new(Event::TermBell(tau_proto::TermBell {})),
                recorded_at: UnixMicros::new(2),
                delivery_id: 2,
                queue_bytes: 1,
                enqueued_at: Instant::now(),
            })
            .expect("blocked producer admitted");
    });
    blocked_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("producer reached byte admission");
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: "worker".to_owned(),
        })
        .expect("local selection");

    let mut scheduler = RendererCommandScheduler::new(remote_rx, local_rx, arbiter);
    assert!(matches!(
        scheduler
            .recv_timeout(Duration::from_millis(10))
            .expect("admitted remote prefix"),
        RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 1,
            ..
        }
    ));
    let selection = scheduler
        .recv_timeout(Duration::from_millis(10))
        .expect("selection remains schedulable");
    let (_term, handle, _input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        crate::tests::cli_test_theme(),
    );
    let RendererCmd::SwitchAgent { agent_id } = selection else {
        panic!("expected selection command");
    };
    renderer.switch_agent(agent_id);
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect(MUTEX_POISONED)
            .as_deref(),
        Some("worker")
    );

    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    harness_stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    send_cancel_prompt_frame(
        &writer,
        &tau_proto::SessionId::parse("session").expect("test session id"),
        Some(tau_proto::AgentId::parse("worker").expect("agent id")),
    )
    .expect("direct cancel uplink");
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    let message = reader
        .read_message()
        .expect("read cancel frame")
        .expect("cancel frame");
    let HarnessInputMessage::Emit(emit) = message else {
        panic!("expected emitted cancel event");
    };
    assert!(matches!(*emit.event, Event::UiCancelPrompt(_)));

    assert_eq!(budget.release(RENDERER_QUEUE_MAX_BYTES), 0);
    producer.join().expect("blocked producer");
    assert!(matches!(
        scheduler
            .recv_timeout(Duration::from_secs(1))
            .expect("later remote arrival"),
        RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: 2,
            ..
        }
    ));
    assert_eq!(budget.release(1), 0);
}

/// Downlink deliveries should attribute bytes to the inner event name so
/// the breakdown points at the event family worth optimizing rather
/// than the transport envelope.
#[test]
fn ui_io_output_message_key_uses_delivered_event_name() {
    let message = HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(tau_client::output_message_key(&message), "term.bell");
}

/// Uplink event emissions should be grouped by the inner event name, not
/// the generic transport envelope, so the debug dump can identify noisy
/// UI-originated event types such as prompt drafts.
#[test]
fn ui_io_input_message_key_uses_emitted_event_name() {
    let message = HarnessInputMessage::emit(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(tau_client::input_message_key(&message), "term.bell");
}

/// Breakdown logging should put the largest contributors first so a noisy
/// one-second sample immediately shows the best optimization target.
#[test]
fn ui_io_breakdown_formats_largest_first() {
    let mut breakdown = BTreeMap::new();
    breakdown.insert("small.event".to_owned(), 512);
    breakdown.insert("large.event".to_owned(), 12 * 1024);

    assert_eq!(
        tau_client::format_protocol_io_breakdown(&breakdown),
        "large.event=12K, small.event=512B"
    );
}

/// The debug stats command uses cumulative counters, so sampling the
/// one-second status buckets must not clear per-event totals or counts.
#[test]
fn ui_io_meter_keeps_cumulative_event_stats_after_sampling() {
    let meter = UiIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(10),
    );
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(15),
    );

    let sample = meter.take_sample();
    assert_eq!(sample.downlink_bytes, 25);
    assert_eq!(
        meter
            .cumulative_stats()
            .downlink
            .get("small.event")
            .copied(),
        Some(tau_client::ProtocolIoFrameStats {
            count: 2,
            bytes: 25
        })
    );
}

/// The human-readable debug dump should sort event types by descending
/// total size and report each event size once, using the same humanized
/// byte format shown elsewhere in the UI I/O diagnostics.
#[test]
fn ui_io_cumulative_stats_format_lists_largest_events_first() {
    let mut stats = UiIoCumulativeStats::default();
    stats.uplink.insert(
        "message.hello".to_owned(),
        tau_client::ProtocolIoFrameStats {
            count: 1,
            bytes: 50,
        },
    );
    stats.downlink.insert(
        "small.event".to_owned(),
        tau_client::ProtocolIoFrameStats {
            count: 3,
            bytes: 512,
        },
    );
    stats.downlink.insert(
        "large.event".to_owned(),
        tau_client::ProtocolIoFrameStats {
            count: 2,
            bytes: 12 * 1024,
        },
    );

    let formatted = format_ui_io_cumulative_stats(&stats);

    assert_eq!(
        formatted,
        "UI event I/O cumulative stats\nuplink: 50B in 1 frame(s)\n  message.hello: 50B count=1\ndownlink: 12K in 5 frame(s)\n  large.event: 12K count=2\n  small.event: 512B count=3"
    );
}

/// The legacy empty UI event stats output is an explicit compatibility
/// contract for the local debug command.
#[test]
fn ui_io_cumulative_stats_format_empty_output_is_stable() {
    assert_eq!(
        format_ui_io_cumulative_stats(&UiIoCumulativeStats::default()),
        "UI event I/O cumulative stats\nuplink: 0B in 0 frame(s)\n  (none)\ndownlink: 0B in 0 frame(s)\n  (none)\nno UI frames recorded yet"
    );
}

/// The interactive debug dump reports cold catch-up separately from steady
/// live delivery and includes exact encoded byte totals.
#[test]
fn ui_io_stats_report_separates_catch_up_and_steady_live() {
    let meter = UiIoMeter::with_diagnostics();
    let record = |message: HarnessOutputMessage| {
        let bytes = tau_proto::encode_message_to_vec(&message)
            .expect("encode test frame")
            .len() as u64;
        meter.record_downlink_frame_bytes(
            &message,
            tau_proto::ProtocolMessageBytes::new(bytes)
                .expect("an encoded protocol fixture is nonempty"),
        );
    };
    record(HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    ));
    record(HarnessOutputMessage::deliver(Event::SessionReplayComplete(
        tau_proto::SessionReplayComplete {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            error: None,
        },
    )));
    record(HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        Event::TermBell(tau_proto::TermBell {}),
    ));
    record(HarnessOutputMessage::deliver_replay(
        UnixMicros::new(3),
        Event::TermBell(tau_proto::TermBell {}),
    ));

    let formatted = format_ui_io_stats(&meter);

    assert!(formatted.contains("cold-attach.replay: bytes="));
    assert!(formatted.contains("cold-attach.non-replay: bytes="));
    assert!(formatted.contains("steady.non-replay: bytes="));
    assert!(formatted.contains("steady.replay: bytes="));
    assert_eq!(formatted.matches("  term.bell: bytes=").count(), 3);
}

/// The command handler should print the same cumulative dump users
/// get interactively and consume the command locally instead of letting it
/// fall through to prompt submission.
#[test]
fn debug_show_ui_event_stats_command_prints_current_counters() {
    let meter = UiIoMeter::default();
    meter.record_bytes(
        tau_client::ProtocolIoDirection::Uplink,
        "ui.prompt_draft".to_owned(),
        Some(42),
    );
    let mut output = Vec::new();

    let handled = handle_debug_show_ui_event_stats_command_text(
        ":debug-show-ui-event-stats",
        &meter,
        |message| output.push(message.to_owned()),
    );

    assert!(handled);
    assert_eq!(
        output,
        vec![
            "UI event I/O cumulative stats\nuplink: 42B in 1 frame(s)\n  ui.prompt_draft: 42B count=1\ndownlink: 0B in 0 frame(s)\n  (none)"
                .to_owned()
        ]
    );
}

/// A mistyped debug stats invocation with arguments should be consumed with
/// a local usage notice rather than becoming an unknown extension action or
/// prompt text.
#[test]
fn debug_show_ui_event_stats_command_rejects_arguments() {
    let meter = UiIoMeter::default();
    let mut output = Vec::new();

    let handled = handle_debug_show_ui_event_stats_command_text(
        ":debug-show-ui-event-stats now",
        &meter,
        |message| output.push(message.to_owned()),
    );

    assert!(handled);
    assert_eq!(
        output,
        vec![":debug-show-ui-event-stats takes no arguments".to_owned()]
    );
}

/// The extension stats command should build a targeted harness debug
/// request instead of falling through to prompt submission, while keeping
/// usage errors local to the UI.
#[test]
fn debug_show_event_stats_command_builds_request() {
    let message = parse_debug_show_event_stats_command(":debug-show-event-stats std-shell")
        .expect("parse command")
        .expect("request message");

    assert_eq!(
        message,
        HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: tau_proto::ExtensionName::parse("std-shell")
                .expect("test identifier must satisfy its grammar")
        })
    );
    assert_eq!(
        parse_debug_show_event_stats_command(":debug-show-event-stats")
            .expect_err("missing extension"),
        DEBUG_SHOW_EVENT_STATS_USAGE
    );
    assert_eq!(
        parse_debug_show_event_stats_command(":debug-show-event-stats std-shell extra")
            .expect_err("extra argument"),
        DEBUG_SHOW_EVENT_STATS_USAGE
    );
}

/// The command's production send path must write exactly one dedicated flat
/// request frame and report it handled so prompt submission cannot see it.
#[test]
fn debug_show_event_stats_command_sends_dedicated_request_frame() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    harness_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let mut usage = Vec::new();

    assert!(handle_debug_show_event_stats_command_text(
        ":debug-show-event-stats std-shell",
        &writer,
        |message| usage.push(message.to_owned()),
    ));

    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert_eq!(
        reader.read_message().expect("read request"),
        Some(HarnessInputMessage::UiDebugEventStatsRequest(
            tau_proto::UiDebugEventStatsRequest {
                extension_name: tau_proto::ExtensionName::parse("std-shell")
                    .expect("test identifier must satisfy its grammar"),
            },
        ))
    );
    assert!(usage.is_empty());
}

/// `:detach` selects the daemon-preserving exit path and writes exactly one
/// dedicated connection-control frame rather than an emitted event.
#[test]
fn detach_command_sends_dedicated_request_frame() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    harness_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));

    assert_eq!(
        handle_ui_detach_command_text(":detach", &writer),
        Some(InputLoopExit::Detach)
    );

    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert_eq!(
        reader.read_message().expect("read request"),
        Some(HarnessInputMessage::UiDetachRequest(
            tau_proto::UiDetachRequest {},
        ))
    );
    match reader.read_message() {
        Err(tau_proto::DecodeError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        other => panic!("unexpected second detach frame: {other:?}"),
    }
}

/// Bare `:tree`'s production command boundary writes exactly one dedicated
/// request frame rather than an emitted event.
#[test]
fn tree_command_sends_dedicated_request_frame() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    harness_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let mut errors = Vec::new();

    assert!(handle_tree_command_text(
        &tau_proto::SessionId::parse("s1").expect("test session id"),
        Some(tau_proto::AgentId::parse("agent-1").expect("agent id")),
        ":tree",
        &writer,
        |message| errors.push(message.to_owned()),
    ));

    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert_eq!(
        reader.read_message().expect("read request"),
        Some(HarnessInputMessage::UiTreeRequest(
            tau_proto::UiTreeRequest {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                target_agent_id: Some(tau_proto::AgentId::parse("agent-1").expect("agent id")),
            },
        ))
    );
    match reader.read_message() {
        Err(tau_proto::DecodeError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        other => panic!("unexpected second tree frame: {other:?}"),
    }
    assert!(errors.is_empty());
}

#[test]
fn handshake_write_error_reads_pending_startup_disconnect() {
    let (mut harness_stdout, ui_stdout) = UnixStream::pair().expect("stdout pair");
    let (ui_stdin, harness_stdin) = UnixStream::pair().expect("stdin pair");
    drop(harness_stdin);

    let mut harness_writer = HarnessOutputWriter::new(&mut harness_stdout);
    harness_writer
        .write_message(&HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some("harness startup failed: missing secret".to_owned()),
        }))
        .expect("write disconnect");
    harness_writer.flush().expect("flush disconnect");

    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stdin, UiIoMeter::default())));
    let mut read_stream: Box<dyn Read + Send> = Box::new(ui_stdout);
    let error = send_handshake_frame(
        &writer,
        &mut read_stream,
        &crate::ui_client::hello_message(
            tau_proto::ExtensionName::parse("tau-chat")
                .expect("chat UI name must satisfy the extension identifier grammar"),
            None,
        ),
    )
    .expect_err("handshake should fail");

    assert!(error.to_string().contains("harness startup failed"));
    assert!(error.to_string().contains("missing secret"));
}
