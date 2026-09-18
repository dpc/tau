use std::collections::BTreeSet;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tau_cli_term::RendererDeliveryId;

use super::{DeliveryMemoryCut, DeliveryMemoryTracker};

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

/// Enabled tracking must move one owner without duplication and release its
/// active entry, while disabled tracking allocates nothing.
#[test]
fn guarded_owner_moves_release_without_changing_disabled_path() {
    let tracker = DeliveryMemoryTracker::new();
    let encoded = tau_proto::ProtocolMessageBytes::new(7).expect("nonzero encoded size");
    tracker.observe_decode(RendererDeliveryId::new(1), &vec!["x".repeat(32)], encoded);
    assert!(tracker.state.lock().expect("tracker").is_none());

    tracker.force_enabled.store(true, Ordering::Relaxed);
    tracker.observe_decode(RendererDeliveryId::new(1), &vec!["x".repeat(32)], encoded);
    tracker.transition(RendererDeliveryId::new(1), DeliveryMemoryCut::ColdStaging);
    tracker.transition(RendererDeliveryId::new(1), DeliveryMemoryCut::RendererFifo);
    tracker.transition(RendererDeliveryId::new(1), DeliveryMemoryCut::Scheduler);
    tracker.transition(RendererDeliveryId::new(1), DeliveryMemoryCut::Handler);
    let state = tracker.state.lock().expect("tracker");
    assert_eq!(state.as_ref().expect("enabled state").active.len(), 1);
    drop(state);
    tracker.release(RendererDeliveryId::new(1));
    assert!(
        tracker
            .state
            .lock()
            .expect("tracker")
            .as_ref()
            .expect("high-water state remains")
            .active
            .is_empty()
    );
}

/// A producer's post-send FIFO acknowledgement must not move ownership
/// backward after the scheduler or handler has already taken the receipt.
#[test]
fn late_renderer_fifo_does_not_override_consumer_ownership() {
    let tracker = DeliveryMemoryTracker::new();
    tracker.force_enable_for_test();
    let encoded = tau_proto::ProtocolMessageBytes::new(7).expect("nonzero encoded size");

    let scheduler_id = RendererDeliveryId::new(1);
    tracker.observe_decode(scheduler_id, &vec!["scheduler"], encoded);
    tracker.transition(scheduler_id, DeliveryMemoryCut::Scheduler);
    tracker.transition(scheduler_id, DeliveryMemoryCut::RendererFifo);
    assert_eq!(
        tracker.cut_for_test(scheduler_id),
        Some(DeliveryMemoryCut::Scheduler)
    );

    let handler_id = RendererDeliveryId::new(2);
    tracker.observe_decode(handler_id, &vec!["handler"], encoded);
    tracker.transition(handler_id, DeliveryMemoryCut::ColdStaging);
    tracker.transition(handler_id, DeliveryMemoryCut::Handler);
    tracker.transition(handler_id, DeliveryMemoryCut::RendererFifo);
    assert_eq!(
        tracker.cut_for_test(handler_id),
        Some(DeliveryMemoryCut::Handler)
    );

    let released_id = RendererDeliveryId::new(3);
    tracker.observe_decode(released_id, &vec!["released"], encoded);
    tracker.release(released_id);
    tracker.transition(released_id, DeliveryMemoryCut::RendererFifo);
    assert_eq!(tracker.cut_for_test(released_id), None);

    let state = tracker.state.lock().expect("tracker");
    assert_eq!(
        state.as_ref().expect("enabled state").high_water_items
            [DeliveryMemoryCut::RendererFifo.index()],
        0,
        "stale transitions must not create false FIFO high-water ownership"
    );
}

/// Both decode/current and cold-staging receipts must retain their normal
/// forward path through FIFO, scheduler, handler, and release.
#[test]
fn normal_renderer_fifo_transitions_remain_valid() {
    let tracker = DeliveryMemoryTracker::new();
    tracker.force_enable_for_test();
    let encoded = tau_proto::ProtocolMessageBytes::new(7).expect("nonzero encoded size");

    for (raw_id, origin) in [
        (1, DeliveryMemoryCut::DecodeCurrent),
        (2, DeliveryMemoryCut::ColdStaging),
    ] {
        let delivery_id = RendererDeliveryId::new(raw_id);
        tracker.observe_decode(delivery_id, &vec!["normal"], encoded);
        if origin == DeliveryMemoryCut::ColdStaging {
            tracker.transition(delivery_id, origin);
        }
        tracker.transition(delivery_id, DeliveryMemoryCut::RendererFifo);
        assert_eq!(
            tracker.cut_for_test(delivery_id),
            Some(DeliveryMemoryCut::RendererFifo)
        );
        tracker.transition(delivery_id, DeliveryMemoryCut::Scheduler);
        tracker.transition(delivery_id, DeliveryMemoryCut::Handler);
        tracker.release(delivery_id);
        assert_eq!(tracker.cut_for_test(delivery_id), None);
    }
}

/// The runtime diagnostic schema must remain a fixed content-free allowlist;
/// adding identities or payload fields requires this privacy oracle to change.
#[test]
fn diagnostic_fields_are_content_free() {
    const FIELDS: &[&str] = &[
        "process",
        "cut",
        "items",
        "owners",
        "encoded_bytes",
        "decoded_logical_bytes_estimate",
        "decoded_requested_capacity_estimate",
        "decoded_containers",
        "expansion_milli",
        "shared_allocations",
        "shared_fanout",
        "high_water_items",
        "high_water_encoded_bytes",
        "high_water_decoded_logical_bytes_estimate",
        "high_water_decoded_requested_capacity_estimate",
        "kernel_bytes_observable",
        "retained_projection_bytes_observable",
    ];
    assert_eq!(FIELDS.len(), 17);
    assert!(FIELDS.iter().all(|field| {
        ![
            "payload",
            "event",
            "agent_id",
            "session_id",
            "prompt_id",
            "delivery_id",
            "cursor",
            "path",
            "model",
            "error",
        ]
        .contains(field)
    }));
}

/// The actual enabled TRACE event must expose aggregate field names while
/// excluding canary payload and process-local delivery identity.
#[test]
fn enabled_trace_output_excludes_payload_and_identity() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer_bytes = Arc::clone(&bytes);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer(move || TraceWriter {
            bytes: Arc::clone(&writer_bytes),
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let tracker = DeliveryMemoryTracker::new();
        let encoded = tau_proto::ProtocolMessageBytes::new(9).expect("encoded bytes");
        tracker.observe_decode(
            RendererDeliveryId::new(8_675_309),
            &vec!["PRIVATE_CANARY_VALUE"],
            encoded,
        );
    });
    let trace = String::from_utf8(bytes.lock().expect("trace bytes").clone()).expect("UTF-8 trace");
    assert!(trace.contains("decoded_requested_capacity_estimate"));
    assert!(trace.contains("decode_current"));
    assert!(!trace.contains("PRIVATE_CANARY_VALUE"));
    assert!(!trace.contains("8675309"));
    assert!(!trace.contains("delivery_id"));
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
        "high_water_items",
        "items",
        "kernel_bytes_observable",
        "owners",
        "process",
        "retained_projection_bytes_observable",
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
