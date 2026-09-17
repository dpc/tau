use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use proptest::prelude::*;

use super::*;
use crate::event_log as path_crate_event_log;

/// Shared byte sink for inspecting actual formatted trace events.
struct TraceWriter {
    /// Captured formatted bytes.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for TraceWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace bytes")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn info(message: &str) -> Event {
    Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.info".to_owned(),
        message: message.to_owned(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    })
}

#[test]
fn append_assigns_sequence_and_timestamp_without_retaining_payloads_in_production() {
    let log = EventLog::new();
    let (seq, recorded_at) = log.append();
    assert_eq!(seq.get(), 0);
    assert!(recorded_at.get() > 0, "append should stamp wall-clock time");
    assert_eq!(log.next_seq().get(), 1);
}

#[test]
fn test_observer_records_committed_events() {
    let log = EventLog::new();
    let (seq, recorded_at) = log.append();
    log.record_for_test(
        seq,
        recorded_at,
        Some(crate::test_connection_id("conn-1")),
        info("hello"),
    );

    let entry = log
        .get_next_from(path_crate_event_log::EventLogSeq::new(0))
        .expect("entry should exist");
    assert_eq!(entry.seq.get(), 0);
    assert_eq!(entry.recorded_at, recorded_at);
    assert_eq!(entry.source, Some(crate::test_connection_id("conn-1")));
}

#[test]
fn get_next_from_skips_earlier_test_observer_entries() {
    let log = EventLog::new();
    for message in ["a", "b", "c"] {
        let (seq, recorded_at) = log.append();
        log.record_for_test(seq, recorded_at, None, info(message));
    }

    let entry = log
        .get_next_from(path_crate_event_log::EventLogSeq::new(1))
        .expect("entry should exist");
    assert_eq!(entry.seq.get(), 1);
    let Event::HarnessNotice(info) = &entry.event else {
        panic!("expected HarnessNotice");
    };
    assert_eq!(info.message, "b");
}

fn routed_notice(message: &str) -> tau_core::RoutedFrame {
    tau_core::RoutedFrame::new(
        None,
        tau_proto::HarnessOutputMessage::deliver(info(message)),
    )
}

/// Retryable one-MiB reads cannot accumulate behind a stalled writer;
/// successful acknowledgement and consumer retirement each release capacity
/// exactly once.
#[test]
fn artifact_egress_is_bounded_until_acknowledgement_or_retirement() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    let target = tau_core::SharedDeliveryTarget::new(log.group(), consumer);
    let frame = || {
        tau_core::RoutedFrame::new(
            None,
            tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(tau_proto::ArtifactResult {
                request_id: "retry".parse().expect("valid request"),
                result: Ok(tau_proto::ArtifactValue::Chunk {
                    offset: 0,
                    bytes: vec![1; tau_proto::ARTIFACT_CHUNK_BYTES],
                    eof: false,
                }),
            })),
        )
    };
    for _ in 0..8 {
        assert_eq!(log.append_egress(frame(), &[target]), vec![target]);
    }
    assert_eq!(log.work().1, (8, 0));
    for _ in 0..32 {
        assert!(log.append_egress(frame(), &[target]).is_empty());
    }
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 8);
    assert_eq!(log.work().1, (8, 0), "rejected append is silent");
    let pending = log.next_egress(consumer).expect("writer ownership");
    // Merely acquiring a frame does not acknowledge it.
    assert!(log.append_egress(frame(), &[target]).is_empty());
    log.acknowledge_egress(consumer, &pending);
    log.acknowledge_egress(consumer, &pending);
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 7);
    assert_eq!(log.work().1, (8, 1), "stale acknowledgement is silent");
    assert_eq!(log.append_egress(frame(), &[target]), vec![target]);
    log.retire_consumer(consumer);
    log.retire_consumer(consumer);
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 0);
    assert_eq!(log.work().1, (10, 2), "repeated retirement is silent");
    assert!(log.inner.lock().expect("log").retained.is_empty());
    let replacement = log.register_consumer();
    let replacement = tau_core::SharedDeliveryTarget::new(log.group(), replacement);
    assert_eq!(
        log.append_egress(frame(), &[replacement]),
        vec![replacement]
    );
}

/// One publication must retain one canonical frame while two independent
/// consumer generations advance, then reclaim it after the slower cursor.
#[test]
fn shared_egress_prunes_only_after_every_consumer_advances() {
    let log = EventLog::new();
    let first = log.register_consumer();
    let second = log.register_consumer();
    let group = log.group();
    let _ = log.append_egress(
        routed_notice("shared"),
        &[
            tau_core::SharedDeliveryTarget::new(group, first),
            tau_core::SharedDeliveryTarget::new(group, second),
        ],
    );
    assert_eq!(log.inner.lock().expect("log").retained.len(), 1);

    let first_pending = log.next_egress(first).expect("first delivery");
    log.acknowledge_egress(first, &first_pending);
    assert_eq!(log.inner.lock().expect("log").retained.len(), 1);

    let second_pending = log.next_egress(second).expect("second delivery");
    log.acknowledge_egress(second, &second_pending);
    assert!(log.inner.lock().expect("log").retained.is_empty());
}

/// Shared-suffix measurement must charge one canonical allocation once while
/// reporting independent attachment and temporary writer ownership as fanout.
#[test]
fn shared_egress_ownership_is_deduplicated_across_attachments() {
    let log = EventLog::new();
    let first = log.register_consumer();
    let second = log.register_consumer();
    let targets = [
        tau_core::SharedDeliveryTarget::new(log.group(), first),
        tau_core::SharedDeliveryTarget::new(log.group(), second),
    ];
    let _ = log.append_egress(routed_notice(&"expanded".repeat(256)), &targets);
    {
        let inner = log.inner.lock().expect("log");
        let position = inner.retained.front().expect("retained position");
        let payload = position.payload.as_ref().expect("shared payload");
        assert_eq!(Arc::strong_count(payload), 1, "one canonical allocation");
        assert_eq!(position.pending_targets.len(), 2, "two attachment owners");
        let estimate =
            tau_delivery_memory::DecodedMemoryEstimate::from_serializable_encoding(&payload.frame)
                .expect("serializable routed frame");
        assert!(estimate.logical_payload_bytes.get() >= 2_048);
        assert!(estimate.requested_capacity_estimate.get() >= estimate.logical_payload_bytes.get());
    }

    let pending = log.next_egress(first).expect("first writer ownership");
    let inner = log.inner.lock().expect("log");
    assert_eq!(
        Arc::strong_count(inner.retained[0].payload.as_ref().expect("payload")),
        2,
        "suffix plus one writer own the same allocation"
    );
    drop(inner);
    log.acknowledge_egress(first, &pending);
}

/// A normal log must not allocate measurement state when its trace is disabled.
#[test]
fn disabled_live_suffix_measurement_allocates_no_observation_state() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    let _ = log.append_egress(
        routed_notice("disabled"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), consumer)],
    );
    assert!(
        log.inner.lock().expect("log").delivery_memory.is_none(),
        "disabled tracing must retain no measurement state"
    );
}

/// The real enabled event-log seams must cache each recursive estimate once,
/// retain attachment/strong-reference high water, and publish a final zero
/// current state after acknowledgement.
#[test]
fn enabled_live_suffix_measurement_tracks_and_releases_real_ownership() {
    let trace_bytes = Arc::new(Mutex::new(Vec::new()));
    let writer_bytes = Arc::clone(&trace_bytes);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer(move || TraceWriter {
            bytes: Arc::clone(&writer_bytes),
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let log = EventLog::new();
        log.force_delivery_memory_for_test();
        let first = log.register_consumer();
        let second = log.register_consumer();
        let targets = [
            tau_core::SharedDeliveryTarget::new(log.group(), first),
            tau_core::SharedDeliveryTarget::new(log.group(), second),
        ];
        let _ = log.append_egress(routed_notice(&"canary".repeat(512)), &targets);
        {
            let inner = log.inner.lock().expect("log");
            let measurement = inner.delivery_memory.as_ref().expect("enabled state");
            assert_eq!(measurement.estimates.len(), 1);
            assert_eq!(measurement.high_shared_allocations, 1);
            assert_eq!(measurement.high_pending_target_fanout, 2);
        }

        let first_pending = log.next_egress(first).expect("first");
        log.acknowledge_egress(first, &first_pending);
        drop(first_pending);
        let second_pending = log.next_egress(second).expect("second");
        log.acknowledge_egress(second, &second_pending);
        drop(second_pending);
        let inner = log.inner.lock().expect("log");
        let measurement = inner.delivery_memory.as_ref().expect("enabled state");
        assert!(
            measurement.estimates.is_empty(),
            "current ownership releases"
        );
        assert!(
            measurement.high_shared_fanout >= 2,
            "writer overlap retained"
        );
        assert_eq!(measurement.high_pending_target_fanout, 2);
    });
    let trace =
        String::from_utf8(trace_bytes.lock().expect("trace bytes").clone()).expect("UTF-8 trace");
    assert!(trace.contains("pending_target_fanout"));
    assert!(trace.contains("high_water_pending_target_fanout"));
    assert!(!trace.contains("canary"));
    assert!(!trace.contains("consumer"));
    let final_record = trace
        .lines()
        .rev()
        .find(|line| line.contains("tau_harness::delivery_memory"))
        .expect("final delivery-memory trace record");
    let fields = final_record
        .split_whitespace()
        .filter_map(|word| word.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let numeric_field = |field| {
        fields
            .get(field)
            .unwrap_or_else(|| panic!("final delivery-memory record has {field}"))
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("final delivery-memory {field} is numeric"))
    };
    for field in [
        "items",
        "encoded_bytes",
        "decoded_logical_bytes_estimate",
        "decoded_requested_capacity_estimate",
        "decoded_containers",
        "expansion_milli",
        "shared_allocations",
        "shared_fanout",
        "pending_target_fanout",
        "overlap_fanout",
    ] {
        assert_eq!(
            numeric_field(field),
            0,
            "final delivery-memory {field} releases current ownership"
        );
    }
    for field in [
        "high_water_encoded_bytes",
        "high_water_decoded_logical_bytes_estimate",
        "high_water_decoded_requested_capacity_estimate",
        "high_water_shared_allocations",
        "high_water_shared_fanout",
        "high_water_pending_target_fanout",
    ] {
        assert!(
            numeric_field(field) > 0,
            "final delivery-memory {field} preserves observed high water"
        );
    }
    let actual = fields.keys().copied().collect::<BTreeSet<_>>();
    let expected = [
        "cut",
        "decoded_containers",
        "decoded_logical_bytes_estimate",
        "decoded_requested_capacity_estimate",
        "encoded_bytes",
        "expansion_milli",
        "high_water_decoded_logical_bytes_estimate",
        "high_water_decoded_requested_capacity_estimate",
        "high_water_encoded_bytes",
        "high_water_pending_target_fanout",
        "high_water_shared_allocations",
        "high_water_shared_fanout",
        "items",
        "kernel_bytes_observable",
        "overlap_fanout",
        "owners",
        "pending_target_fanout",
        "process",
        "shared_allocations",
        "shared_fanout",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "actual trace schema is an exact allowlist"
    );
}

/// A connected generation that does not acknowledge a targeted frame must pin
/// shared retention until lifecycle retirement, never trigger implicit expiry.
#[test]
fn stalled_consumer_pins_until_explicit_retirement() {
    let log = EventLog::new();
    let stalled = log.register_consumer();
    let group = log.group();
    let _ = log.append_egress(
        routed_notice("pinned"),
        &[tau_core::SharedDeliveryTarget::new(group, stalled)],
    );
    let _pending = log.next_egress(stalled).expect("stalled delivery");
    assert_eq!(log.inner.lock().expect("log").retained.len(), 1);

    log.retire_consumer(stalled);
    assert!(log.inner.lock().expect("log").retained.is_empty());
}

/// A terminal close must release replay pause, deliver only through its
/// captured tail, and retire without retaining a later targeted frame.
#[test]
fn close_after_current_releases_pause_and_excludes_later_frames() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    let target = tau_core::SharedDeliveryTarget::new(log.group(), consumer);
    log.set_catch_up_paused(consumer, true);
    let _ = log.append_egress(routed_notice("before close"), &[target]);

    log.close_consumer_after_current(consumer);
    let _ = log.append_egress(routed_notice("after close"), &[target]);

    let pending = log
        .next_egress(consumer)
        .expect("close releases pause for the captured frame");
    assert_eq!(pending.seq.0, 0);
    log.acknowledge_egress(consumer, &pending);
    assert!(
        log.next_egress(consumer).is_none(),
        "the captured tail retires before the later targeted frame"
    );
    let inner = log.inner.lock().expect("log");
    assert!(!inner.consumers.contains_key(&consumer));
    assert!(
        inner.retained.is_empty(),
        "retirement releases the post-boundary target and continuity metadata"
    );
}

/// A cursor stalled on an earlier targeted position may retain lightweight
/// continuity metadata but must not pin a later payload after that payload's
/// independent frozen target acknowledges it.
#[test]
fn unrelated_stalled_cursor_does_not_pin_later_payload() {
    let log = EventLog::new();
    let stalled = log.register_consumer();
    let healthy = log.register_consumer();
    let group = log.group();
    let _ = log.append_egress(
        routed_notice("stalled"),
        &[tau_core::SharedDeliveryTarget::new(group, stalled)],
    );
    let _ = log.append_egress(
        routed_notice("healthy"),
        &[tau_core::SharedDeliveryTarget::new(group, healthy)],
    );

    let healthy_pending = log.next_egress(healthy).expect("healthy delivery");
    log.acknowledge_egress(healthy, &healthy_pending);
    let inner = log.inner.lock().expect("log");
    assert_eq!(
        inner.retained.len(),
        2,
        "stalled cursor preserves positions"
    );
    assert!(
        inner.retained[1].payload.is_none(),
        "later payload releases after its own target advances"
    );
    assert!(inner.retained[0].payload.is_some());
}

/// Replacing a connection must create a new generation at the current tail so
/// it cannot inherit or acknowledge the retired generation's obligation.
#[test]
fn replacement_generation_starts_at_live_tail() {
    let log = EventLog::new();
    let old = log.register_consumer();
    let group = log.group();
    let _ = log.append_egress(
        routed_notice("old"),
        &[tau_core::SharedDeliveryTarget::new(group, old)],
    );
    log.retire_consumer(old);
    let replacement = log.register_consumer();
    assert_eq!(
        log.inner
            .lock()
            .expect("log")
            .consumers
            .get(&replacement)
            .expect("replacement")
            .cursor
            .0,
        1
    );
}

/// A sparse target must advance across the complete non-target run with one
/// minimum-cursor pass, one memory observation, and one wakeup.
#[test]
fn sparse_target_scan_batches_cursor_prune_observation_and_notification() {
    let log = EventLog::new();
    let consumers = (0..5).map(|_| log.register_consumer()).collect::<Vec<_>>();
    let selected = consumers[0];
    for _ in 0..64 {
        let _ = log.append_egress(routed_notice("untargeted"), &[]);
    }
    let _ = log.append_egress(
        routed_notice("selected"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), selected)],
    );

    log.reset_work();
    let pending = log.next_egress(selected).expect("sparse target");
    assert_eq!(pending.seq.0, 64);
    let (work, notifications) = log.work();
    assert_eq!(
        work,
        EventLogWork {
            prune_calls: 1,
            prune_consumer_visits: 5,
            observe_calls: 1,
            scan_position_visits: 65,
            catch_up_waits: 0,
            tail_waits: 0,
            ..EventLogWork::default()
        }
    );
    assert_eq!(notifications, (0, 1));
}

/// Wait-entry observations share the predicate mutex, so the caller cannot
/// mistake thread startup for a thread actually entering its condition wait.
fn wait_for_work(log: &EventLog, ready: impl Fn(&EventLogWork) -> bool) {
    let inner = log.inner.lock().expect("log");
    let (inner, _) = log
        .waiter_entered
        .wait_timeout_while(inner, Duration::from_secs(2), |inner| !ready(&inner.work))
        .expect("wait entry");
    assert!(ready(&inner.work), "waiter did not enter predicate wait");
}

/// Append must still wake non-target followers to advance continuity, whereas
/// selection and unrelated skips must not release a target's real flush
/// barrier. Count notification calls, never OS wake returns (which may be
/// spurious).
#[test]
fn follower_and_progress_notifications_preserve_flush_barriers() {
    let log = EventLog::new();
    let a = log.register_consumer();
    let b = log.register_consumer();
    let follower_a = {
        let log = Arc::clone(&log);
        std::thread::spawn(move || log.next_egress(a))
    };
    assert!(log.wait_for_tail_wait(1, Duration::from_secs(2)));
    let follower_b = {
        let log = Arc::clone(&log);
        std::thread::spawn(move || log.next_egress(b))
    };
    // The mutex also serializes either follower's final tail predicate with
    // this append; correctness does not depend on whether B parked already.
    let _ = log.append_egress(
        routed_notice("only A"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), a)],
    );
    let pending = follower_a.join().expect("A follower").expect("A target");
    // B publishes its progress notification after unlocking, then enters the
    // tail wait. Observe both the cursor and notification before checking
    // counts, including when append beat B's first predicate check.
    {
        let inner = log.inner.lock().expect("log");
        let (inner, _) = log
            .waiter_entered
            .wait_timeout_while(inner, Duration::from_secs(2), |inner| {
                inner.consumers[&b].cursor.0 != 1
                    || log.progress_notifications.load(Ordering::Relaxed) != 1
            })
            .expect("B progress");
        assert_eq!(inner.consumers[&b].cursor.0, 1);
        assert_eq!(log.progress_notifications.load(Ordering::Relaxed), 1);
    }
    assert_eq!(log.work().1, (1, 1), "append then non-target skip");

    let (tx, rx) = sync_channel(2);
    let flushers = (0..2)
        .map(|_| {
            let log = Arc::clone(&log);
            let tx = tx.clone();
            std::thread::spawn(move || {
                log.flush_consumer(a);
                tx.send(()).expect("flush completion");
            })
        })
        .collect::<Vec<_>>();
    wait_for_work(&log, |work| work.flush_waiters == 2);
    assert!(rx.try_recv().is_err(), "selection did not acknowledge A");
    log.acknowledge_egress(a, &pending);
    for flusher in flushers {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("flush released");
        flusher.join().expect("flusher");
    }
    assert_eq!(log.work().1, (1, 2), "ack notifies only progress");
    log.acknowledge_egress(a, &pending);
    assert_eq!(log.work().1, (1, 2), "stale ack is silent");
    log.retire_consumer_after_io(b);
    assert!(follower_b.join().expect("B follower").is_none());
    assert_eq!(log.work().1, (2, 3));
}

/// Both lifecycle wait classes must finish whether retirement precedes wait
/// entry or follows it; paused followers must also observe resume and close.
#[test]
fn lifecycle_controls_notify_only_their_predicate_classes() {
    for close in [false, true] {
        let log = EventLog::new();
        let consumer = log.register_consumer();
        assert_eq!(log.work().1, (0, 0));
        log.set_catch_up_paused(consumer, true);
        let follower = {
            let log = Arc::clone(&log);
            std::thread::spawn(move || log.next_egress(consumer))
        };
        assert!(log.wait_for_catch_up_wait(1, Duration::from_secs(2)));
        let retirement = {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                log.wait_for_consumer_retirement(consumer, Duration::from_secs(2))
            })
        };
        wait_for_work(&log, |work| work.retirement_waits >= 1);
        if close {
            log.close_consumer_after_current(consumer);
        } else {
            log.set_catch_up_paused(consumer, false);
            assert!(log.wait_for_tail_wait(1, Duration::from_secs(2)));
            log.retire_consumer_after_io(consumer);
        }
        assert!(follower.join().expect("follower").is_none());
        assert!(retirement.join().expect("retirement waiter"));
        assert_eq!(log.work().1, (3, 1));
        assert!(log.wait_for_consumer_retirement(consumer, Duration::ZERO));
        log.flush_consumer(consumer);
        log.retire_consumer(consumer);
        log.retire_consumer_after_io(consumer);
        log.close_consumer_after_current(consumer);
        log.set_catch_up_paused(consumer, false);
        assert_eq!(log.work().1, (3, 1), "absent controls are silent");
    }
}

/// Append cannot change an active minimum; ahead-of-front cursor changes and
/// retirement cannot unpin it. Retirement visits only the cursor's suffix and
/// repeated retirement does no work, while the last minimum prunes immediately.
#[test]
fn bookkeeping_elides_only_provably_irrelevant_work() {
    let log = EventLog::new();
    let slow = log.register_consumer();
    for _ in 0..16 {
        let _ = log.append_egress(routed_notice("old prefix"), &[]);
    }
    let fast = log.register_consumer();
    let target = tau_core::SharedDeliveryTarget::new(log.group(), fast);
    log.reset_work();
    let _ = log.append_egress(routed_notice("fast"), &[target]);
    assert_eq!(log.work().0.prune_consumer_visits, 0);
    let pending = log.next_egress(fast).expect("fast target");
    log.acknowledge_egress(fast, &pending);
    let _ = log.append_egress(routed_notice("skip"), &[]);
    let _ = log.append_egress(routed_notice("fast again"), &[target]);
    let pending = log.next_egress(fast).expect("next fast target");
    assert_eq!(pending.seq.0, 18);
    assert_eq!(log.work().0.prune_calls, 0, "ahead ack and skip");
    log.retire_consumer_after_io(fast);
    let (work, _) = log.work();
    assert_eq!(work.prune_calls, 0);
    assert_eq!(work.retirement_position_visits, 1);
    assert_eq!(log.inner.lock().expect("log").retained.len(), 19);
    log.reset_work();
    log.retire_consumer_after_io(fast);
    assert_eq!(log.work(), (EventLogWork::default(), (0, 0)));
    log.retire_consumer_after_io(slow);
    assert!(log.inner.lock().expect("log").retained.is_empty());
    assert_eq!(log.work().0.prune_calls, 1);
    log.reset_work();
    let _ = log.append_egress(routed_notice("no consumers"), &[target]);
    assert!(log.inner.lock().expect("log").retained.is_empty());
    assert_eq!(log.work().0.prune_calls, 0);
    assert_eq!(log.work().1, (1, 0));
}

/// A tied minimum still needs the full minimum scan: its first advance cannot
/// prune, but advancing the final owner must immediately reclaim the prefix.
#[test]
fn tied_minimum_prunes_only_after_its_final_owner_advances() {
    let log = EventLog::new();
    let consumers = [log.register_consumer(), log.register_consumer()];
    let targets = consumers.map(|id| tau_core::SharedDeliveryTarget::new(log.group(), id));
    let _ = log.append_egress(routed_notice("shared"), &targets);
    log.reset_work();
    for (index, consumer) in consumers.into_iter().enumerate() {
        let pending = log.next_egress(consumer).expect("shared target");
        log.acknowledge_egress(consumer, &pending);
        assert_eq!(
            log.inner.lock().expect("log").retained.len(),
            usize::from(index == 0)
        );
    }
    assert_eq!(log.work().0.prune_calls, 2);
    assert_eq!(log.work().0.prune_consumer_visits, 4);
    assert_eq!(log.work().1, (0, 2));
}

/// Close retirement must visit post-boundary artifact targets too, but charge
/// release belongs only to the final target even across duplicate cleanup.
#[test]
fn close_retirement_releases_shared_artifact_charge_exactly_once() {
    let log = EventLog::new();
    let slow = log.register_consumer();
    let _ = log.append_egress(routed_notice("pinned prefix"), &[]);
    let closing = log.register_consumer();
    log.close_consumer_after_current(closing);
    let targets = [slow, closing].map(|id| tau_core::SharedDeliveryTarget::new(log.group(), id));
    let _ = log.append_egress(
        tau_core::RoutedFrame::new(
            None,
            tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(tau_proto::ArtifactResult {
                request_id: "post-close".parse().expect("request ID"),
                result: Ok(tau_proto::ArtifactValue::Chunk {
                    offset: 0,
                    bytes: vec![1],
                    eof: true,
                }),
            })),
        ),
        &targets,
    );
    log.reset_work();
    assert!(log.next_egress(closing).is_none());
    assert_eq!(log.work().0.retirement_position_visits, 1);
    assert_eq!(log.work().0.prune_calls, 0);
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 1);
    log.retire_consumer_after_io(closing);
    assert_eq!(log.work().0.retirement_position_visits, 1);
    let pending = log.next_egress(slow).expect("remaining artifact target");
    log.acknowledge_egress(slow, &pending);
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 0);
    log.retire_consumer_after_io(slow);
    assert_eq!(log.inner.lock().expect("log").artifact_egress_count, 0);
}

/// Reaching a captured close boundary through only non-target positions must
/// retire in the same batch and release a later frozen payload.
#[test]
fn sparse_close_boundary_retires_with_one_prune_observation_and_notification() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    for _ in 0..32 {
        let _ = log.append_egress(routed_notice("before close"), &[]);
    }
    log.close_consumer_after_current(consumer);
    let _ = log.append_egress(
        routed_notice("after close"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), consumer)],
    );

    log.reset_work();
    assert!(log.next_egress(consumer).is_none());
    let (work, notifications) = log.work();
    assert_eq!(
        work,
        EventLogWork {
            prune_calls: 1,
            prune_consumer_visits: 0,
            observe_calls: 1,
            scan_position_visits: 32,
            catch_up_waits: 0,
            tail_waits: 0,
            retirement_position_visits: 33,
            ..EventLogWork::default()
        }
    );
    assert_eq!(notifications, (1, 1));
    assert!(log.inner.lock().expect("log").retained.is_empty());
}

/// A follower that batches to the live tail must resume after a later append
/// instead of missing the append notification between lock acquisitions.
#[test]
fn sparse_tail_scan_resumes_for_later_target() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    for _ in 0..32 {
        let _ = log.append_egress(routed_notice("before tail"), &[]);
    }
    log.reset_work();
    let (tx, rx) = sync_channel(1);
    let follower = {
        let log = Arc::clone(&log);
        std::thread::spawn(move || {
            let _ = tx.send(log.next_egress(consumer));
        })
    };
    if !log.wait_for_tail_wait(1, Duration::from_secs(1)) {
        log.retire_consumer_after_io(consumer);
        let _ = follower.join();
        panic!("follower did not batch to the tail and enter its wait");
    }
    let _ = log.append_egress(
        routed_notice("after tail"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), consumer)],
    );
    let pending = match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            follower.join().expect("follower thread");
            panic!("follower retired before returning the later target");
        }
        Err(error) => {
            log.retire_consumer_after_io(consumer);
            let _ = follower.join();
            panic!("follower did not return the later target: {error}");
        }
    };
    follower.join().expect("follower thread");
    assert_eq!(pending.seq.0, 32);
    let (work, notifications) = log.work();
    assert_eq!(work.prune_calls, 1);
    assert_eq!(work.prune_consumer_visits, 1);
    assert_eq!(work.observe_calls, 3);
    assert_eq!(work.scan_position_visits, 33);
    assert_eq!(notifications, (1, 1));
}

/// A flush barrier remains behind a selected frame until successful
/// acknowledgement even when selection skipped a sparse prefix in one batch.
#[test]
fn sparse_scan_preserves_flush_acknowledgement_barrier() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    for _ in 0..16 {
        let _ = log.append_egress(routed_notice("untargeted"), &[]);
    }
    let _ = log.append_egress(
        routed_notice("barrier"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), consumer)],
    );
    let pending = log.next_egress(consumer).expect("barrier target");
    assert_eq!(
        log.inner
            .lock()
            .expect("log")
            .consumers
            .get(&consumer)
            .expect("consumer")
            .cursor
            .0,
        pending.seq.0,
        "selection must not cross the write/flush acknowledgement barrier"
    );
    log.acknowledge_egress(consumer, &pending);
    log.flush_consumer(consumer);
    assert_eq!(
        log.inner
            .lock()
            .expect("log")
            .consumers
            .get(&consumer)
            .expect("consumer")
            .cursor
            .0,
        17
    );
}

/// Retiring the slow minimum cursor after another consumer batches a sparse
/// prefix must prune that prefix while preserving the fast consumer's target.
#[test]
fn sparse_scan_preserves_consumer_retirement_and_minimum_cursor_pruning() {
    let log = EventLog::new();
    let fast = log.register_consumer();
    let slow = log.register_consumer();
    for _ in 0..16 {
        let _ = log.append_egress(routed_notice("untargeted"), &[]);
    }
    let targets = [
        tau_core::SharedDeliveryTarget::new(log.group(), fast),
        tau_core::SharedDeliveryTarget::new(log.group(), slow),
    ];
    let _ = log.append_egress(routed_notice("shared target"), &targets);
    let pending = log.next_egress(fast).expect("fast target");
    assert_eq!(pending.seq.0, 16);
    assert_eq!(log.inner.lock().expect("log").retained.len(), 17);

    log.reset_work();
    log.retire_consumer(slow);
    let (work, notifications) = log.work();
    assert_eq!(work.prune_calls, 1);
    assert_eq!(work.prune_consumer_visits, 1);
    assert_eq!(work.observe_calls, 1);
    assert_eq!(notifications, (1, 1));
    let inner = log.inner.lock().expect("log");
    assert_eq!(inner.retained.len(), 1);
    assert_eq!(inner.retained[0].seq.0, 16);
    assert!(inner.retained[0].pending_targets.contains(&fast));
    drop(inner);

    log.acknowledge_egress(fast, &pending);
    assert!(log.inner.lock().expect("log").retained.is_empty());
}

/// Replay pause must keep the cursor fixed; release may then batch a sparse
/// live suffix without changing target visibility.
#[test]
fn replay_pause_release_batches_sparse_live_suffix() {
    let log = EventLog::new();
    let consumer = log.register_consumer();
    log.set_catch_up_paused(consumer, true);
    for _ in 0..24 {
        let _ = log.append_egress(routed_notice("buffered live"), &[]);
    }
    let _ = log.append_egress(
        routed_notice("visible after replay"),
        &[tau_core::SharedDeliveryTarget::new(log.group(), consumer)],
    );
    let (tx, rx) = sync_channel(1);
    let follower = {
        let log = Arc::clone(&log);
        std::thread::spawn(move || {
            let _ = tx.send(log.next_egress(consumer));
        })
    };
    if !log.wait_for_catch_up_wait(1, Duration::from_secs(1)) {
        log.retire_consumer_after_io(consumer);
        let _ = follower.join();
        panic!("follower did not enter the replay-pause wait before release");
    }
    assert_eq!(
        log.inner
            .lock()
            .expect("log")
            .consumers
            .get(&consumer)
            .expect("consumer")
            .cursor
            .0,
        0
    );
    log.set_catch_up_paused(consumer, false);
    let pending = match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            follower.join().expect("follower thread");
            panic!("follower retired before returning the post-replay target");
        }
        Err(error) => {
            log.retire_consumer_after_io(consumer);
            let _ = follower.join();
            panic!("follower did not return the post-replay target: {error}");
        }
    };
    follower.join().expect("follower thread");
    assert_eq!(pending.seq.0, 24);
}

/// Manual work benchmark demonstrates that sparse scanning performs `U`
/// position checks but only one `C`-consumer minimum pass and one observation.
#[test]
#[ignore = "manual sparse EventLog asymptotic work benchmark"]
fn benchmark_sparse_egress_scan_work() {
    for consumer_count in [1_u64, 8, 64] {
        for untargeted_count in [100_u64, 1_000, 10_000] {
            let log = EventLog::new();
            let consumers = (0..consumer_count)
                .map(|_| log.register_consumer())
                .collect::<Vec<_>>();
            for _ in 0..untargeted_count {
                let _ = log.append_egress(routed_notice("untargeted"), &[]);
            }
            let _ = log.append_egress(
                routed_notice("target"),
                &[tau_core::SharedDeliveryTarget::new(
                    log.group(),
                    consumers[0],
                )],
            );
            log.reset_work();
            let started = Instant::now();
            let pending = log.next_egress(consumers[0]).expect("target");
            let elapsed = started.elapsed();
            assert_eq!(pending.seq.0, untargeted_count);
            let (work, notifications) = log.work();
            assert_eq!(work.scan_position_visits, untargeted_count + 1);
            assert_eq!(work.prune_calls, 1);
            assert_eq!(work.prune_consumer_visits, consumer_count);
            assert_eq!(work.observe_calls, 1);
            assert_eq!(notifications, (0, 1));
            eprintln!(
                "sparse EventLog scan: U={untargeted_count} C={consumer_count} \
                 position_visits={} consumer_visits={} observations={} elapsed={elapsed:?}",
                work.scan_position_visits, work.prune_consumer_visits, work.observe_calls
            );
        }
    }
}

proptest! {
    /// Random connect, publish, advance, and retirement traces must agree with a
    /// small single-consumer cursor model after every observable transition.
    #[test]
    fn randomized_cursor_traces_match_reference_model(actions in prop::collection::vec(0_u8..5, 1..128)) {
        let log = EventLog::new();
        let group = log.group();
        let mut consumer = Some(log.register_consumer());
        let mut tail = 0_u64;
        let mut cursor = 0_u64;
        let mut targeted = Vec::<(u64, bool)>::new();

        for action in actions {
            match action {
                0 | 1 => {
                    let is_targeted = action == 0 && consumer.is_some();
                    let targets = consumer
                        .filter(|_| is_targeted)
                        .map(|consumer| vec![tau_core::SharedDeliveryTarget::new(group, consumer)])
                        .unwrap_or_default();
                    let _ = log.append_egress(routed_notice("model"), &targets);
                    targeted.push((tail, is_targeted));
                    tail = tail.saturating_add(1);
                }
                2 => {
                    if let Some(current) = consumer
                        && targeted
                            .iter()
                            .any(|(seq, is_targeted)| cursor <= *seq && *is_targeted)
                    {
                        let pending = log.next_egress(current).expect("modeled target");
                        cursor = pending.seq.0.saturating_add(1);
                        log.acknowledge_egress(current, &pending);
                    }
                }
                3 => {
                    if let Some(current) = consumer.take() {
                        log.retire_consumer(current);
                    }
                    cursor = tail;
                }
                _ => {
                    if consumer.is_none() {
                        consumer = Some(log.register_consumer());
                        cursor = tail;
                    }
                }
            }

            let inner = log.inner.lock().expect("model snapshot");
            let expected_first = if consumer.is_some() { cursor } else { tail };
            let expected_retained = tail.saturating_sub(expected_first) as usize;
            prop_assert_eq!(inner.next_egress_seq.0, tail);
            prop_assert_eq!(inner.retained.len(), expected_retained);
            if let Some(current) = consumer {
                prop_assert_eq!(
                    inner.consumers.get(&current).expect("live model consumer").cursor.0,
                    cursor
                );
            } else {
                prop_assert!(inner.consumers.is_empty());
            }
        }
    }

    /// Random live publication, delivery, pause, retirement, replacement and
    /// close traces must preserve frozen generations, including post-close
    /// obligations, and exact minimum-cursor retention after every transition.
    #[test]
    fn randomized_multi_consumer_live_replay_matches_reference_model(
        actions in prop::collection::vec(0_u8..14, 1..192)
    ) {
        let log = EventLog::new();
        let mut consumers = [Some(log.register_consumer()), Some(log.register_consumer())];
        let group = log.group();
        let mut tail = 0_u64;
        let mut cursors = [0_u64; 2];
        let mut paused = [false; 2];
        let mut close_after = [None; 2];
        let mut target_masks = Vec::<u8>::new();

        for action in actions {
            match action {
                mask @ 0..=3 => {
                    let targets = consumers
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .filter_map(|(_, consumer)| {
                            consumer.map(|consumer| tau_core::SharedDeliveryTarget::new(group, consumer))
                        })
                        .collect::<Vec<_>>();
                    let _ = log.append_egress(routed_notice("model"), &targets);
                    let active_mask = consumers.iter().enumerate().fold(0, |mask, (index, consumer)| {
                        mask | if consumer.is_some() { 1 << index } else { 0 }
                    });
                    target_masks.push(mask & active_mask);
                    tail = tail.saturating_add(1);
                }
                deliver @ 4..=5 => {
                    let index = usize::from(deliver - 4);
                    if let Some(consumer) = consumers[index]
                        && !paused[index]
                    {
                        let boundary = close_after[index].unwrap_or(tail);
                        let expected = target_masks
                            .iter()
                            .enumerate()
                            .take(usize::try_from(boundary).expect("boundary fits usize"))
                            .skip(usize::try_from(cursors[index]).expect("cursor fits usize"))
                            .find_map(|(seq, mask)| {
                                (mask & (1 << index) != 0)
                                    .then(|| u64::try_from(seq).expect("sequence fits u64"))
                            });
                        if let Some(expected) = expected {
                            let pending = log.next_egress(consumer).expect("modeled target");
                            prop_assert_eq!(pending.seq.0, expected);
                            log.acknowledge_egress(consumer, &pending);
                            cursors[index] = expected.saturating_add(1);
                        } else if close_after[index].is_some() {
                            prop_assert!(log.next_egress(consumer).is_none());
                            consumers[index] = None;
                            for mask in &mut target_masks {
                                *mask &= !(1 << index);
                            }
                        }
                    }
                }
                toggle @ 6..=7 => {
                    let index = usize::from(toggle - 6);
                    if let Some(consumer) = consumers[index] {
                        paused[index] = !paused[index];
                        log.set_catch_up_paused(consumer, paused[index]);
                    }
                }
                retire @ 8..=9 => {
                    let index = usize::from(retire - 8);
                    if let Some(consumer) = consumers[index].take() {
                        log.retire_consumer_after_io(consumer);
                        // Repeated writer/lifecycle cleanup is deliberately inert.
                        log.retire_consumer_after_io(consumer);
                        for mask in &mut target_masks {
                            *mask &= !(1 << index);
                        }
                    }
                }
                replace @ 10..=11 => {
                    let index = usize::from(replace - 10);
                    if let Some(consumer) = consumers[index].take() {
                        log.retire_consumer_after_io(consumer);
                    }
                    for mask in &mut target_masks {
                        *mask &= !(1 << index);
                    }
                    consumers[index] = Some(log.register_consumer());
                    cursors[index] = tail;
                    paused[index] = false;
                    close_after[index] = None;
                }
                close @ 12..=13 => {
                    let index = usize::from(close - 12);
                    if let Some(consumer) = consumers[index] {
                        log.close_consumer_after_current(consumer);
                        close_after[index] = Some(tail);
                        paused[index] = false;
                        // Sink cleanup must not steal terminal close ownership.
                        log.retire_consumer(consumer);
                    }
                }
                _ => unreachable!("action strategy is bounded"),
            }

            let inner = log.inner.lock().expect("model snapshot");
            let first = consumers.iter().enumerate()
                .filter(|(_, consumer)| consumer.is_some())
                .map(|(index, _)| cursors[index]).min().unwrap_or(tail);
            prop_assert_eq!(inner.next_egress_seq.0, tail);
            prop_assert_eq!(
                inner.retained.front().map(|position| position.seq.0),
                (first < tail).then_some(first)
            );
            prop_assert_eq!(
                inner.retained.len(),
                usize::try_from(tail.saturating_sub(first)).expect("retained length fits usize")
            );
            prop_assert_eq!(inner.consumers.len(), consumers.iter().flatten().count());
            for (index, consumer) in consumers.iter().enumerate() {
                let Some(consumer) = consumer else { continue; };
                let state = inner.consumers.get(consumer).expect("modeled consumer");
                prop_assert_eq!(state.cursor.0, cursors[index]);
                prop_assert_eq!(state.catch_up_paused, paused[index]);
                prop_assert_eq!(state.close_after.map(|position| position.0), close_after[index]);
            }
            for position in &inner.retained {
                let expected_pending = consumers
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        target_masks
                            [usize::try_from(position.seq.0).expect("sequence fits usize")]
                            & (1 << index)
                            != 0
                            && cursors[*index] <= position.seq.0
                    })
                    .filter_map(|(_, consumer)| *consumer)
                    .collect::<HashSet<_>>();
                prop_assert_eq!(&position.pending_targets, &expected_pending);
                prop_assert_eq!(position.payload.is_some(), !expected_pending.is_empty());
            }
        }
    }
}
