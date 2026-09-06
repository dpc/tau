use std::cell::Cell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{atomic as path_std_sync_atomic, mpsc};

use tau_cli_term::RendererDeliveryId;
use tau_proto::HarnessOutputWriter;

use super::delivery_memory::DeliveryMemoryTracker;
use super::*;

const TEST_DELIVERY_ID_ONE: RendererDeliveryId = RendererDeliveryId::new(1);
const TEST_DELIVERY_ID_TWO: RendererDeliveryId = RendererDeliveryId::new(2);
const TEST_DELIVERY_ID_THREE: RendererDeliveryId = RendererDeliveryId::new(3);

/// Renderer delivery allocation starts at one and keeps each normal delivery
/// distinct instead of sharing a counter with queue sizes or timestamps.
#[test]
fn renderer_delivery_allocator_starts_at_one_and_advances() {
    let next_delivery_id = Cell::new(1);

    assert_eq!(allocate_renderer_delivery_id(&next_delivery_id).get(), 1);
    assert_eq!(allocate_renderer_delivery_id(&next_delivery_id).get(), 2);
}

/// Renderer delivery allocation must fail at the integer boundary rather than
/// silently reusing the final identity for later deliveries.
#[test]
#[should_panic(expected = "renderer delivery identity exhausted")]
fn renderer_delivery_allocator_rejects_exhaustion_without_reusing_maximum() {
    let next_delivery_id = Cell::new(u64::MAX);

    let _ = allocate_renderer_delivery_id(&next_delivery_id);
}

/// Creates production-shaped renderer channels for focused scheduler tests.
fn renderer_scheduler_channels(
    remote_capacity: usize,
    admitted: Arc<path_std_sync_atomic::AtomicU64>,
    arbiter: Arc<Mutex<()>>,
) -> (
    RemoteRendererSender,
    LocalRendererSender,
    RendererCommandScheduler,
) {
    let (wake_tx, wake_rx) = tau_blocking_notify_channel::channel();
    let (remote_tx, remote_rx) = RemoteRendererSender::channel(remote_capacity, wake_tx.clone());
    let (local_tx, local_rx) =
        LocalRendererSender::channel(admitted.clone(), arbiter.clone(), wake_tx);
    let scheduler =
        RendererCommandScheduler::new(remote_rx, local_rx, admitted, arbiter, wake_rx, None);
    (remote_tx, local_tx, scheduler)
}

/// Startup must not open command input until the renderer has applied the
/// harness-owned ordinary-quit prediction.
#[test]
fn initial_quit_disposition_barrier_waits_for_renderer_application() {
    let (applied_tx, applied_rx) = mpsc::sync_channel(1);
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        waiting_tx.send(()).expect("waiter started");
        await_initial_quit_disposition(&applied_rx, Duration::from_secs(1))
    });

    waiting_rx.recv().expect("waiter entered barrier");
    assert!(
        !waiter.is_finished(),
        "barrier returned before renderer acknowledgement"
    );
    applied_tx.send(Ok(())).expect("renderer acknowledgement");
    assert!(waiter.join().expect("waiter").is_ok());
}

/// The initial projection follows the remote renderer FIFO rather than updating
/// shared completion state directly from the socket reader.
#[test]
fn initial_quit_disposition_uses_remote_renderer_fifo() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, _local_tx, mut scheduler) =
        renderer_scheduler_channels(1, admitted.clone(), arbiter.clone());
    let (applied_tx, _applied_rx) = mpsc::sync_channel(1);

    enqueue_remote_quit_disposition(
        tau_proto::UiQuitDisposition::Terminating,
        Some(applied_tx),
        &remote_tx,
        &arbiter,
        &admitted,
    )
    .expect("enqueue quit disposition");

    assert_eq!(admitted.load(Ordering::Acquire), 1);
    let RendererCmd::UiQuitDispositionChanged {
        disposition,
        initial_applied,
    } = scheduler
        .recv_timeout(Duration::from_secs(1))
        .expect("remote projection")
    else {
        panic!("expected remote quit disposition");
    };
    assert_eq!(disposition, tau_proto::UiQuitDisposition::Terminating);
    assert!(initial_applied.is_some());
}

/// Builds one ordinary remote command for scheduler ordering tests.
fn remote_bell(delivery_id: u64) -> RendererCmd {
    RendererCmd::Remote {
        abandoned_shell_starts: Vec::new(),
        presentation: cold_attach_stager::RendererPresentation::Ordinary,
        event: Box::new(Event::TermBell(tau_proto::TermBell {})),
        recorded_at: UnixMicros::new(delivery_id),
        delivery_id: RendererDeliveryId::new(delivery_id),
        queue_bytes: 1,
        enqueued_at: Instant::now(),
        folded_frames: Vec::new(),
    }
}

/// Admission retains buffered bytes so an acknowledgement coalesced with a
/// later disconnect is still visible to the long-lived reader.
#[test]
fn ui_session_admission_preserves_coalesced_followup() {
    let (client, server) = UnixStream::pair().expect("socket pair");
    let expected = tau_proto::SessionId::parse("session-1").expect("valid session id");
    let mut writer = tau_proto::HarnessOutputWriter::new(BufWriter::new(server));
    writer
        .write_message(&HarnessOutputMessage::SessionAccepted(
            tau_proto::SessionAccepted {
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

/// One folded projection releases and diagnoses every original item and byte
/// permit exactly once, while the caller performs only one projection.
#[test]
fn folded_remote_release_accounts_for_every_original_frame_once() {
    let budget = RendererByteBudget::new();
    let queued_items = path_std_sync_atomic::AtomicUsize::new(3);
    budget.acquire(2);
    budget.acquire(3);
    budget.acquire(5);
    let mut diagnosed = Vec::new();
    let mut projections = 0;

    release_remote_queue_frames(
        RendererQueueFrame {
            delivery_id: RendererDeliveryId::new(11),
            queue_bytes: 2,
            enqueued_at: Instant::now(),
        },
        vec![
            RendererQueueFrame {
                delivery_id: RendererDeliveryId::new(12),
                queue_bytes: 3,
                enqueued_at: Instant::now(),
            },
            RendererQueueFrame {
                delivery_id: RendererDeliveryId::new(13),
                queue_bytes: 5,
                enqueued_at: Instant::now(),
            },
        ],
        &queued_items,
        &budget,
        |delivery_id| diagnosed.push(delivery_id),
    );
    projections += 1;

    assert_eq!(
        diagnosed
            .into_iter()
            .map(RendererDeliveryId::get)
            .collect::<Vec<_>>(),
        vec![11, 12, 13]
    );
    assert_eq!(queued_items.load(Ordering::Acquire), 0);
    assert_eq!(*budget.used.lock().expect(MUTEX_POISONED), 0);
    assert_eq!(projections, 1);
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
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(2));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(4, admitted.clone(), arbiter);
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: TEST_DELIVERY_ID_ONE,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
        })
        .expect("remote delivery");
    remote_tx
        .send(RendererCmd::RemoteDisconnect {
            reason: Some("done".to_owned()),
            delivery_id: TEST_DELIVERY_ID_TWO,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
        })
        .expect("remote disconnect");
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: agent_id("worker"),
            intent_epoch: 1,
        })
        .expect("local selection");
    local_tx
        .send(RendererCmd::ActionInvoked {
            invocation_id: tau_proto::ActionInvocationId::parse("action-test")
                .expect("test identifier must satisfy its grammar"),
            owner_agent_id: Some(agent_id("worker")),
        })
        .expect("local action");

    let remote = scheduler
        .recv_timeout(Duration::from_millis(10))
        .expect("remote delivery");
    assert!(matches!(
        remote,
        RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        }
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::RemoteDisconnect {
            delivery_id: TEST_DELIVERY_ID_TWO,
            ..
        })
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::SwitchAgent { agent_id, .. }) if agent_id.as_str() == "worker"
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_millis(10)),
        Ok(RendererCmd::ActionInvoked {
            invocation_id,
            owner_agent_id: Some(owner),
        }) if invocation_id.as_ref() == "action-test" && owner.as_str() == "worker"
    ));
    assert!(!scheduler.remote_closed());
    drop(remote_tx);
}

/// A decoded event keeps its original box through ordinary staging, bounded
/// admission, and scheduler ownership.
#[test]
fn renderer_admission_preserves_decoded_event_box() {
    let (wake_tx, wake_rx) = tau_blocking_notify_channel::channel();
    let (remote_tx, remote_rx) = RemoteRendererSender::channel(1, wake_tx.clone());
    let remote_admitted = path_std_sync_atomic::AtomicU64::new(0);
    let queued_items = path_std_sync_atomic::AtomicUsize::new(0);
    let arbiter = Mutex::new(());
    let budget = RendererByteBudget::new();
    let source =
        tau_proto::EventDelivery::live(UnixMicros::new(1), Event::TermBell(tau_proto::TermBell {}));
    let source_allocation = std::ptr::from_ref(source.event.as_ref());
    let delivery =
        renderer_event_from_delivery(source, 1, TEST_DELIVERY_ID_ONE).expect("ordinary delivery");
    let mut stager = cold_attach_stager::ColdAttachStager::pass_through();
    let delivery = stager.admit(delivery).pop().expect("staged delivery");
    let delivery_memory = Arc::new(DeliveryMemoryTracker::new());
    delivery_memory.force_enable_for_test();
    delivery_memory.observe_decode(
        TEST_DELIVERY_ID_ONE,
        &tau_proto::HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {})),
        tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte"),
    );

    assert!(enqueue_remote_delivery(
        delivery,
        Some(delivery_memory.as_ref()),
        &remote_tx,
        &budget,
        &queued_items,
        &arbiter,
        &remote_admitted,
    ));

    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let (local_tx, local_rx) =
        LocalRendererSender::channel(admitted.clone(), Arc::new(Mutex::new(())), wake_tx);
    drop(local_tx);
    let mut scheduler = RendererCommandScheduler::new(
        remote_rx,
        local_rx,
        admitted,
        Arc::new(Mutex::new(())),
        wake_rx,
        Some(Arc::clone(&delivery_memory)),
    );
    assert_eq!(
        delivery_memory.cut_for_test(TEST_DELIVERY_ID_ONE),
        Some(DeliveryMemoryCut::RendererFifo)
    );
    let RendererCmd::Remote { event, .. } = scheduler
        .recv_timeout(Duration::from_millis(10))
        .expect("admitted remote delivery")
    else {
        panic!("expected remote delivery");
    };
    assert_eq!(std::ptr::from_ref(event.as_ref()), source_allocation);
    assert_eq!(
        delivery_memory.cut_for_test(TEST_DELIVERY_ID_ONE),
        Some(DeliveryMemoryCut::Scheduler)
    );
}

/// Enabled folded-handler coordination must retain every source estimate
/// through Handler and release all receipts without changing folded output.
#[test]
fn enabled_folded_handler_ownership_releases_every_source() {
    let memory = DeliveryMemoryTracker::new();
    memory.force_enable_for_test();
    let encoded = tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte");
    for id in [1, 2] {
        memory.observe_decode(
            RendererDeliveryId::new(id),
            &tau_proto::HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {})),
            encoded,
        );
        memory.transition(RendererDeliveryId::new(id), DeliveryMemoryCut::Scheduler);
    }
    let folded = vec![RendererQueueFrame {
        delivery_id: TEST_DELIVERY_ID_TWO,
        queue_bytes: 1,
        enqueued_at: Instant::now(),
    }];
    let receipts = begin_remote_memory_handler(Some(&memory), TEST_DELIVERY_ID_ONE, &folded);
    assert_eq!(
        receipts
            .iter()
            .copied()
            .map(RendererDeliveryId::get)
            .collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(
        memory.cut_for_test(TEST_DELIVERY_ID_ONE),
        Some(DeliveryMemoryCut::Handler)
    );
    assert_eq!(
        memory.cut_for_test(TEST_DELIVERY_ID_TWO),
        Some(DeliveryMemoryCut::Handler)
    );
    finish_remote_memory_handler(Some(&memory), TEST_DELIVERY_ID_ONE, receipts);
    assert_eq!(memory.active_len_for_test(), 0);
}

/// Cold filtering and renderer admission failure must release exactly their
/// enabled receipts while preserving the forwarded delivery.
#[test]
fn enabled_cold_filter_and_enqueue_failure_release_receipts() {
    let memory = DeliveryMemoryTracker::new();
    memory.force_enable_for_test();
    let encoded = tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte");
    for id in [1, 2] {
        memory.observe_decode(
            RendererDeliveryId::new(id),
            &tau_proto::HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {})),
            encoded,
        );
        memory.transition(RendererDeliveryId::new(id), DeliveryMemoryCut::ColdStaging);
    }
    let forwarded = renderer_event_from_delivery(
        tau_proto::EventDelivery::live(UnixMicros::new(2), Event::TermBell(tau_proto::TermBell {})),
        1,
        TEST_DELIVERY_ID_TWO,
    )
    .expect("forwarded delivery");
    release_filtered_cold_memory(
        Some(&memory),
        vec![TEST_DELIVERY_ID_ONE, TEST_DELIVERY_ID_TWO],
        std::slice::from_ref(&forwarded),
    );
    assert_eq!(memory.cut_for_test(TEST_DELIVERY_ID_ONE), None);
    assert_eq!(
        memory.cut_for_test(TEST_DELIVERY_ID_TWO),
        Some(DeliveryMemoryCut::ColdStaging)
    );

    let (wake_tx, _wake_rx) = tau_blocking_notify_channel::channel();
    let (remote_tx, remote_rx) = RemoteRendererSender::channel(1, wake_tx);
    drop(remote_rx);
    assert!(!enqueue_remote_delivery(
        forwarded,
        Some(&memory),
        &remote_tx,
        &RendererByteBudget::new(),
        &path_std_sync_atomic::AtomicUsize::new(0),
        &Mutex::new(()),
        &path_std_sync_atomic::AtomicU64::new(0),
    ));
    assert_eq!(memory.active_len_for_test(), 0);
}

/// Real cold admission must move a replayed transcript row into ColdStaging
/// before the replay boundary forwards it unchanged.
#[test]
fn enabled_real_cold_admission_tracks_retention_and_forwarding() {
    let memory = DeliveryMemoryTracker::new();
    memory.force_enable_for_test();
    let event = Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
        literal: false,
        session_id: "session-1".parse().expect("session id"),
        text: "cold".to_owned(),
        agent_id: agent_id("agent-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    memory.observe_decode(
        RendererDeliveryId::new(41),
        &tau_proto::HarnessOutputMessage::deliver(event.clone()),
        tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte"),
    );
    let delivery = renderer_event_from_delivery(
        tau_proto::EventDelivery::replay(UnixMicros::new(1), event),
        1,
        RendererDeliveryId::new(41),
    )
    .expect("replay delivery");
    let mut stager = cold_attach_stager::ColdAttachStager::staging();
    assert!(admit_cold_delivery(&mut stager, delivery, Some(&memory)).is_empty());
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(41)),
        Some(DeliveryMemoryCut::ColdStaging)
    );
    let forwarded = stager.finish_before_disconnect();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].delivery_id.get(), 41);
}

/// Disconnect coordination must hold its enabled receipt through handler
/// completion and release it afterward.
#[test]
fn enabled_disconnect_handler_releases_receipt() {
    let memory = DeliveryMemoryTracker::new();
    memory.force_enable_for_test();
    memory.observe_decode(
        RendererDeliveryId::new(9),
        &tau_proto::HarnessOutputMessage::Disconnect(tau_proto::Disconnect { reason: None }),
        tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte"),
    );
    memory.transition(RendererDeliveryId::new(9), DeliveryMemoryCut::Scheduler);
    begin_disconnect_memory(Some(&memory), RendererDeliveryId::new(9));
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(9)),
        Some(DeliveryMemoryCut::Handler)
    );
    finish_disconnect_memory(Some(&memory), RendererDeliveryId::new(9));
    assert_eq!(memory.active_len_for_test(), 0);
}

/// A remote reservation captured by a local watermark must not be
/// overtaken even when its channel send completes after the local send.
#[test]
fn renderer_scheduler_waits_for_reserved_remote_arriving_after_local() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(2, admitted.clone(), arbiter);
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: agent_id("worker"),
            intent_epoch: 1,
        })
        .expect("local selection");
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: TEST_DELIVERY_ID_ONE,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
        })
        .expect("reserved remote");

    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::SwitchAgent { agent_id, .. }) if agent_id.as_str() == "worker"
    ));
}

/// Local enqueue cannot linearize between the scheduler's local-empty check
/// and its remote dequeue because both operations share the admission
/// arbiter.
#[test]
fn renderer_scheduler_serializes_local_capture_with_remote_dequeue() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(1, admitted, arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: TEST_DELIVERY_ID_ONE,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
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
                agent_id: agent_id("worker"),
                intent_epoch: 1,
            })
            .expect("local enqueue");
        done_tx.send(()).expect("local enqueue done");
    });

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
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("local enqueue completes after remote dequeue");
    assert!(matches!(
        scheduler.recv_timeout(Duration::from_secs(1)),
        Ok(RendererCmd::SwitchAgent { agent_id, .. }) if agent_id.as_str() == "worker"
    ));
    sender.join().expect("local sender");
}

/// A remote enqueue immediately after the scheduler's empty scan must leave a
/// retained wake, not wait for the caller's deadline.
#[test]
fn renderer_scheduler_wakes_for_remote_arriving_after_empty_scan() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) = renderer_scheduler_channels(1, admitted, arbiter);
    let (result_tx, result_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut after_empty_scan = || remote_tx.send(remote_bell(1)).expect("remote arrival");
        let result =
            scheduler.recv_timeout_before_wait(Duration::from_secs(30), &mut after_empty_scan);
        result_tx.send(result).expect("scheduler result");
    });
    let result = match result_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(result) => result,
        Err(error) => {
            local_tx
                .send(RendererCmd::SetEmptyTarget {
                    intent_epoch: 1,
                    target: EmptyUiTarget::Overview,
                })
                .expect("cleanup local wake");
            worker.join().expect("scheduler worker");
            panic!("remote arrival did not wake scheduler: {error}");
        }
    };
    assert!(matches!(
        result,
        Ok(RendererCmd::Remote {
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    worker.join().expect("scheduler worker");
}

/// A local enqueue immediately after the scheduler's empty scan must wake the
/// same shared wait source while retaining local command order.
#[test]
fn renderer_scheduler_wakes_for_local_arriving_after_empty_scan() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) = renderer_scheduler_channels(1, admitted, arbiter);
    let (result_tx, result_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut after_empty_scan = || {
            local_tx
                .send(RendererCmd::SetEmptyTarget {
                    intent_epoch: 1,
                    target: EmptyUiTarget::Overview,
                })
                .expect("local arrival");
        };
        let result =
            scheduler.recv_timeout_before_wait(Duration::from_secs(30), &mut after_empty_scan);
        result_tx.send(result).expect("scheduler result");
    });
    let result = match result_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(result) => result,
        Err(error) => {
            remote_tx.send(remote_bell(1)).expect("cleanup remote wake");
            worker.join().expect("scheduler worker");
            panic!("local arrival did not wake scheduler: {error}");
        }
    };
    assert!(matches!(
        result,
        Ok(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview
        })
    ));
    worker.join().expect("scheduler worker");
}

/// Coalesced simultaneous remote and local notifications must rerun the
/// existing watermark arbiter, so reserved remote work cannot be overtaken.
#[test]
fn renderer_scheduler_coalesced_wake_preserves_remote_reservation() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(1, admitted.clone(), arbiter);
    let mut after_empty_scan = || {
        admitted.fetch_add(1, Ordering::AcqRel);
        remote_tx.send(remote_bell(1)).expect("reserved remote");
        local_tx
            .send(RendererCmd::SetEmptyTarget {
                intent_epoch: 1,
                target: EmptyUiTarget::Overview,
            })
            .expect("watermarked local");
    };

    assert!(matches!(
        scheduler.recv_timeout_before_wait(Duration::from_secs(1), &mut after_empty_scan),
        Ok(RendererCmd::Remote {
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    assert!(matches!(
        scheduler.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview
        })
    ));
}

/// Closing an idle remote source must neither spin nor hide later local work;
/// the shared wake preserves the independent local channel lifetime.
#[test]
fn renderer_scheduler_remote_close_waits_for_local_or_deadline() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) = renderer_scheduler_channels(1, admitted, arbiter);
    drop(remote_tx);

    assert!(matches!(
        scheduler.recv_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    local_tx
        .send(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview,
        })
        .expect("local after remote close");
    assert!(matches!(
        scheduler.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview
        })
    ));
}

/// Dropping both producer families must wake the scheduler and report actual
/// command-channel disconnection rather than treating wake closure as
/// authority.
#[test]
fn renderer_scheduler_reports_disconnect_after_both_sources_close() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) = renderer_scheduler_channels(1, admitted, arbiter);
    drop(remote_tx);
    drop(local_tx);

    assert!(matches!(
        scheduler.recv_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}

/// Retained or repeated stale wakes must not extend the caller's deadline; the
/// scheduler checks elapsed time after its final arbiter pass and before wait.
#[test]
fn renderer_scheduler_stale_wakes_cannot_extend_deadline() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (wake_tx, wake_rx) = tau_blocking_notify_channel::channel();
    let (_remote_tx, remote_rx) = RemoteRendererSender::channel(1, wake_tx.clone());
    let (_local_tx, local_rx) =
        LocalRendererSender::channel(admitted.clone(), arbiter.clone(), wake_tx.clone());
    let mut scheduler =
        RendererCommandScheduler::new(remote_rx, local_rx, admitted, arbiter, wake_rx, None);
    let mut hook_calls = 0;
    let mut before_each_wait = || {
        hook_calls += 1;
        if hook_calls <= 2 {
            wake_tx.notify();
        }
    };

    assert!(matches!(
        scheduler.recv_timeout_before_each_wait(Duration::ZERO, &mut before_each_wait),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(
        hook_calls, 1,
        "an elapsed deadline must return after one final arbiter pass"
    );
}

/// Exhaustively models two enqueue-then-notify producers and a coalescing
/// consumer to prevent reordering that can strand eligible work after parking.
#[test]
fn renderer_scheduler_enqueue_then_notify_model_has_no_lost_wake() {
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Producer {
        Idle,
        NotificationOwed,
        Done,
    }

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    struct State {
        work: u8,
        wake: bool,
        parked: bool,
        producers: [Producer; 2],
    }

    let initial = State {
        work: 0,
        wake: false,
        parked: false,
        producers: [Producer::Idle; 2],
    };
    let mut pending = VecDeque::from([initial]);
    let mut visited = HashSet::new();
    while let Some(state) = pending.pop_front() {
        if !visited.insert(state) {
            continue;
        }
        let notification_owed = state.producers.contains(&Producer::NotificationOwed);
        assert!(
            !(state.parked && state.work > 0 && !state.wake && !notification_owed),
            "eligible work was stranded without a retained or owed wake: {state:?}"
        );

        for producer_index in 0..state.producers.len() {
            let mut next = state;
            match state.producers[producer_index] {
                Producer::Idle => {
                    next.work += 1;
                    next.producers[producer_index] = Producer::NotificationOwed;
                }
                Producer::NotificationOwed => {
                    next.wake = true;
                    next.parked = false;
                    next.producers[producer_index] = Producer::Done;
                }
                Producer::Done => continue,
            }
            pending.push_back(next);
        }

        let mut consumer = state;
        if consumer.work > 0 {
            consumer.work -= 1;
            consumer.parked = false;
        } else if consumer.wake {
            consumer.wake = false;
            consumer.parked = false;
        } else {
            consumer.parked = true;
        }
        pending.push_back(consumer);
    }
}

/// An action ownership command must run after its captured older prefix
/// but before a result delivery admitted later.
#[test]
fn renderer_scheduler_places_action_before_later_remote_result() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(4, admitted.clone(), arbiter);
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: TEST_DELIVERY_ID_ONE,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
        })
        .expect("older remote");
    local_tx
        .send(RendererCmd::ActionInvoked {
            invocation_id: tau_proto::ActionInvocationId::parse("action-test")
                .expect("test identifier must satisfy its grammar"),
            owner_agent_id: Some(agent_id("worker")),
        })
        .expect("local action");
    admitted.store(2, Ordering::Release);
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(2),
            delivery_id: TEST_DELIVERY_ID_TWO,
            queue_bytes: 1,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
        })
        .expect("later result");

    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    assert!(matches!(next(), Ok(RendererCmd::ActionInvoked { .. })));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_TWO,
            ..
        })
    ));
}

/// Later remote arrivals must not starve a local command once its finite
/// admission watermark has been drained.
#[test]
fn renderer_scheduler_bounds_local_progress_under_remote_replenishment() {
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(2));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(8, admitted.clone(), arbiter);
    local_tx
        .send(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview,
        })
        .expect("local selection");
    for delivery_id in 1..=4 {
        admitted.store(delivery_id, Ordering::Release);
        remote_tx
            .send(RendererCmd::Remote {
                abandoned_shell_starts: Vec::new(),
                presentation: cold_attach_stager::RendererPresentation::Ordinary,
                event: Box::new(Event::TermBell(tau_proto::TermBell {})),
                recorded_at: UnixMicros::new(delivery_id),
                delivery_id: RendererDeliveryId::new(delivery_id),
                queue_bytes: 1,
                enqueued_at: Instant::now(),
                folded_frames: Vec::new(),
            })
            .expect("remote delivery");
    }

    let mut next = || scheduler.recv_timeout(Duration::from_millis(10));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_ONE,
            ..
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_TWO,
            ..
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::SetEmptyTarget {
            intent_epoch: 1,
            target: EmptyUiTarget::Overview
        })
    ));
    assert!(matches!(
        next(),
        Ok(RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_THREE,
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
    let admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(1));
    let arbiter = Arc::new(Mutex::new(()));
    let (remote_tx, local_tx, mut scheduler) =
        renderer_scheduler_channels(1, admitted.clone(), arbiter.clone());
    remote_tx
        .send(RendererCmd::Remote {
            abandoned_shell_starts: Vec::new(),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            event: Box::new(Event::TermBell(tau_proto::TermBell {})),
            recorded_at: UnixMicros::new(1),
            delivery_id: TEST_DELIVERY_ID_ONE,
            queue_bytes: RENDERER_QUEUE_MAX_BYTES,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
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
                delivery_id: TEST_DELIVERY_ID_TWO,
                queue_bytes: 1,
                enqueued_at: Instant::now(),
                folded_frames: Vec::new(),
            })
            .expect("blocked producer admitted");
    });
    blocked_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("producer reached byte admission");
    local_tx
        .send(RendererCmd::SwitchAgent {
            agent_id: agent_id("worker"),
            intent_epoch: 1,
        })
        .expect("local selection");

    assert!(matches!(
        scheduler
            .recv_timeout(Duration::from_millis(10))
            .expect("admitted remote prefix"),
        RendererCmd::Remote {
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            delivery_id: TEST_DELIVERY_ID_ONE,
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
    let RendererCmd::SwitchAgent { agent_id, .. } = selection else {
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
            delivery_id: TEST_DELIVERY_ID_TWO,
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

/// `:quit-session` selects canonical session shutdown and writes exactly one
/// dedicated lifecycle request rather than an emitted event.
#[test]
fn quit_session_command_sends_dedicated_request_frame() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    harness_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));

    assert_eq!(
        handle_ui_shutdown_command_text(":quit-session", &writer).expect("send shutdown request"),
        Some(InputLoopExit::QuitSession)
    );

    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert_eq!(
        reader.read_message().expect("read request"),
        Some(HarnessInputMessage::UiShutdownRequest(
            tau_proto::UiShutdownRequest {},
        ))
    );
    assert!(!InputLoopExit::QuitSession.detaches());
}

/// Command parsing must distinguish explicit detach from ordinary quit so the
/// common teardown handshake can clear daemon policy before disconnecting.
#[test]
fn detach_selects_policy_clearing_exit() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));

    assert_eq!(
        handle_ui_detach_command_text(":detach"),
        Some(InputLoopExit::Detach)
    );
    assert!(!InputLoopExit::Quit.detaches());
    assert!(InputLoopExit::Detach.detaches());
    drop(writer);
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert_eq!(reader.read_message().expect("read EOF"), None);
}

/// UI teardown must send explicit detach before waiting for its authoritative
/// reply; an ordinary quit uses the same exchange without clearing the policy.
#[test]
fn exit_handshake_distinguishes_detach_and_waits_for_authority() {
    for (exit, detach) in [(InputLoopExit::Quit, false), (InputLoopExit::Detach, true)] {
        let (ui, harness) = UnixStream::pair().expect("socket pair");
        let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
        let (reply_tx, reply_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness));
            let Some(HarnessInputMessage::UiQuitRequest(request)) =
                reader.read_message().expect("request")
            else {
                panic!("expected quit request");
            };
            assert_eq!(request.detach, detach);
            // A stale reply from a timed-out request cannot decide this quit.
            reply_tx
                .send(tau_proto::UiQuitResult {
                    request_id: "stale-quit".to_owned(),
                    disposition: tau_proto::UiQuitDisposition::Terminating,
                })
                .expect("stale reply");
            reply_tx
                .send(tau_proto::UiQuitResult {
                    request_id: request.request_id,
                    disposition: tau_proto::UiQuitDisposition::Detached,
                })
                .expect("ack");
        });
        assert_eq!(
            request_ui_quit(&writer, &reply_rx, exit.detaches()),
            Some(tau_proto::UiQuitDisposition::Detached)
        );
        peer.join().expect("peer");
    }
}

/// EOF or a failed acknowledgment channel cannot be upgraded into a successful
/// detach decision by the UI.
#[test]
fn exit_handshake_without_acknowledgment_is_unconfirmed() {
    let (ui, _harness) = UnixStream::pair().expect("socket pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui, UiIoMeter::default())));
    let (tx, rx) = mpsc::channel();
    drop(tx);
    assert_eq!(request_ui_quit(&writer, &rx, true), None);
    assert_eq!(
        request_ui_exit(InputLoopExit::Quit, &writer, &rx, true),
        None
    );
}

/// A failed owned shutdown must not print the same success status as a clean
/// process exit, even though the child has definitely stopped.
#[test]
fn failed_owned_daemon_exit_is_not_successful_termination() {
    let mut child = Command::new("sh")
        .args(["-c", "exit 9"])
        .spawn()
        .expect("spawn isolated failing child");
    child.wait().expect("reap isolated child");
    let daemon = DaemonHandle::Owned {
        child: Some(child),
        harness_path: PathBuf::from("unused-test-harness"),
        initial_ui: None,
    };
    assert_eq!(
        finish_daemon_for_exit(
            Some(tau_proto::UiQuitDisposition::Terminating),
            daemon,
            None
        ),
        Err("daemon exited with an error"),
    );
}

/// An attached UI without an exact process observer cannot certify termination
/// merely because it sent a shutdown request or observed socket EOF.
#[test]
fn attached_exit_without_observer_is_unconfirmed() {
    let daemon = DaemonHandle::Attached {
        harness_path: PathBuf::from("unused-test-harness"),
    };
    assert!(
        finish_daemon_for_exit(
            Some(tau_proto::UiQuitDisposition::Terminating),
            daemon,
            None
        )
        .is_err()
    );
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
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
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

/// A withheld optional roster response cannot block terminal or renderer
/// startup; publication occurs only after the lookup thread completes.
#[test]
fn optional_attach_roster_lookup_runs_off_startup_path() {
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let (result_tx, result_rx) = mpsc::sync_channel(1);

    let worker = spawn_optional_attach_roster(
        move || {
            release_rx.recv().expect("release lookup");
            Ok(Vec::new())
        },
        move |result| result_tx.send(result).expect("publish roster result"),
    );

    assert!(matches!(
        result_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release_tx.send(()).expect("release lookup");
    assert_eq!(
        result_rx.recv().expect("roster result"),
        Ok(Vec::<tau_proto::SessionAgentListEntry>::new())
    );
    worker.join().expect("roster worker");
}
