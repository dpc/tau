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
        assert!(estimate.logical_payload_bytes >= 2_048);
        assert!(estimate.requested_capacity_estimate >= estimate.logical_payload_bytes);
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
        }
    );
    assert_eq!(notifications, 1);
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
        }
    );
    assert_eq!(notifications, 1);
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
    assert_eq!(work.prune_calls, 2);
    assert_eq!(work.prune_consumer_visits, 2);
    assert_eq!(work.observe_calls, 3);
    assert_eq!(work.scan_position_visits, 33);
    assert_eq!(notifications, 2);
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
    assert_eq!(notifications, 1);
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
            assert_eq!(notifications, 1);
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

    /// Random two-consumer live publication, delivery, and replay-pause traces
    /// must preserve independent cursors, target visibility, and minimum-cursor
    /// retention after every transition.
    #[test]
    fn randomized_multi_consumer_live_replay_matches_reference_model(
        actions in prop::collection::vec(0_u8..8, 1..192)
    ) {
        let log = EventLog::new();
        let consumers = [log.register_consumer(), log.register_consumer()];
        let group = log.group();
        let mut tail = 0_u64;
        let mut cursors = [0_u64; 2];
        let mut paused = [false; 2];
        let mut target_masks = Vec::<u8>::new();

        for action in actions {
            match action {
                mask @ 0..=3 => {
                    let targets = consumers
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .map(|(_, consumer)| {
                            tau_core::SharedDeliveryTarget::new(group, *consumer)
                        })
                        .collect::<Vec<_>>();
                    let _ = log.append_egress(routed_notice("model"), &targets);
                    target_masks.push(mask);
                    tail = tail.saturating_add(1);
                }
                deliver @ 4..=5 => {
                    let index = usize::from(deliver - 4);
                    if !paused[index]
                        && let Some(expected) = target_masks
                            .iter()
                            .enumerate()
                            .skip(usize::try_from(cursors[index]).expect("cursor fits usize"))
                            .find_map(|(seq, mask)| {
                                (mask & (1 << index) != 0)
                                    .then(|| u64::try_from(seq).expect("sequence fits u64"))
                            })
                    {
                        let pending = log
                            .next_egress(consumers[index])
                            .expect("modeled target");
                        prop_assert_eq!(pending.seq.0, expected);
                        log.acknowledge_egress(consumers[index], &pending);
                        cursors[index] = expected.saturating_add(1);
                    }
                }
                toggle @ 6..=7 => {
                    let index = usize::from(toggle - 6);
                    paused[index] = !paused[index];
                    log.set_catch_up_paused(consumers[index], paused[index]);
                }
                _ => unreachable!("action strategy is bounded"),
            }

            let inner = log.inner.lock().expect("model snapshot");
            let first = cursors.into_iter().min().expect("two cursors");
            prop_assert_eq!(inner.next_egress_seq.0, tail);
            prop_assert_eq!(
                inner.retained.front().map(|position| position.seq.0),
                (first < tail).then_some(first)
            );
            prop_assert_eq!(
                inner.retained.len(),
                usize::try_from(tail.saturating_sub(first)).expect("retained length fits usize")
            );
            for (index, consumer) in consumers.iter().enumerate() {
                let state = inner.consumers.get(consumer).expect("modeled consumer");
                prop_assert_eq!(state.cursor.0, cursors[index]);
                prop_assert_eq!(state.catch_up_paused, paused[index]);
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
                    .map(|(_, consumer)| *consumer)
                    .collect::<HashSet<_>>();
                prop_assert_eq!(&position.pending_targets, &expected_pending);
                prop_assert_eq!(position.payload.is_some(), !expected_pending.is_empty());
            }
        }
    }
}
