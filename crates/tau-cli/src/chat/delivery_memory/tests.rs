use std::collections::BTreeSet;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

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
    tracker.observe_decode(1, &vec!["x".repeat(32)], encoded);
    assert!(tracker.state.lock().expect("tracker").is_none());

    tracker.force_enabled.store(true, Ordering::Relaxed);
    tracker.observe_decode(1, &vec!["x".repeat(32)], encoded);
    tracker.transition(1, DeliveryMemoryCut::ColdStaging);
    tracker.transition(1, DeliveryMemoryCut::RendererFifo);
    tracker.transition(1, DeliveryMemoryCut::Scheduler);
    tracker.transition(1, DeliveryMemoryCut::Handler);
    let state = tracker.state.lock().expect("tracker");
    assert_eq!(state.as_ref().expect("enabled state").active.len(), 1);
    drop(state);
    tracker.release(1);
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
        tracker.observe_decode(8_675_309, &vec!["PRIVATE_CANARY_VALUE"], encoded);
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
