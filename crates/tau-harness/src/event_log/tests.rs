use std::collections::BTreeSet;
use std::io::Write;

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
    let actual = trace
        .split_whitespace()
        .filter_map(|word| word.split_once('=').map(|(field, _)| field))
        .collect::<BTreeSet<_>>();
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
}
