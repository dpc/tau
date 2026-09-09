use std::hint::black_box;
use std::time::Instant;

use serde_json::Value;

use super::*;
use crate::private_attempt_trace::{AttemptTrace, Backend, Outcome, Transport};

/// The lean schema must preserve nullable milestones, true zero values, and
/// remain far below its independent 8 KiB admission ceiling.
#[test]
fn bounded_record_preserves_null_and_zero() {
    let session_id = "session-timing".parse().expect("session id");
    let agent_prompt_id = "ap-timing".parse().expect("prompt id");
    let provider_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 8,
        response_received_tokens: 2,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                requests: 99,
                sent_tokens: 999,
                ..tau_proto::TokenUsageCounts::default()
            },
            ..tau_proto::TokenUsageStats::default()
        },
        ..tau_proto::ProviderTokenUsage::default()
    };
    let bytes = record(
        CaptureMetadata {
            session_id: &session_id,
            agent_prompt_id: &agent_prompt_id,
            model: "test-model",
            profile: Some("test-profile"),
            operation: "inference",
            logical_attempt: Some(2),
            attempt_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            final_wire_dispatch_index: Some(1),
            repair_reason: "none",
            facts: AttemptFacts {
                tool_enabled: Some(false),
                usage: Some(ResponseUsage::from_provider(&provider_usage)),
                ..AttemptFacts::default()
            },
        },
        AttemptTiming {
            backend: "codex",
            transport: "websocket",
            outcome: "completed",
            dispatch_origin: "ws_enqueue",
            total_us: 12,
            prepare_us: 0,
            lowering_us: 0,
            serialization_us: 0,
            capture_us: 0,
            pool_wait_us: 0,
            connect_upgrade_us: 0,
            enqueue_us: 0,
            decode_us: 0,
            dispatch_to_first_input_us: Some(0),
            dispatch_to_first_associated_event_us: None,
            dispatch_to_first_text_delta_us: None,
            dispatch_to_first_reasoning_delta_us: None,
            dispatch_to_first_actionable_item_us: None,
            dispatch_to_first_semantic_us: None,
            dispatch_to_terminal_us: Some(10),
            terminal_to_return_us: Some(2),
            request_bytes_total: 0,
            first_input_bytes: 0,
            dispatch_count: 1,
            decode_count: 0,
            connection_state: "new",
        },
    )
    .expect("bounded timing record");
    assert!(bytes.len() < MAX_RECORD_BYTES);
    let value: Value = serde_json::from_slice(&bytes).expect("timing JSON");
    assert_eq!(
        value["timings_us"]["final_dispatch_to_first_owner_dequeued_input"],
        0
    );
    assert_eq!(
        value["timings_us"]["final_dispatch_to_first_associated_event"],
        Value::Null
    );
    assert_eq!(value["coverage"]["first_owner_dequeued_input"], "observed");
    assert_eq!(value["coverage"]["first_associated_event"], "not_observed");
    assert_eq!(value["coverage"]["tail"], "not_observed_after_owner_return");
    assert_eq!(value["facts"]["usage"]["prompt_sent_tokens"], 10);
    assert!(value["facts"]["usage"].get("stats").is_none());
    assert!(value["facts"]["usage"].get("model").is_none());
    assert!(value["facts"]["usage"].get("by_model").is_none());
}

/// The closed scalar facts cannot expand one record beyond its admission
/// bound or inject an arbitrary adapter payload.
#[test]
fn closed_scalar_record_submits_within_bound() {
    let session_id = "session-timing".parse().expect("session id");
    let agent_prompt_id = "ap-timing".parse().expect("prompt id");
    let timing = AttemptTiming {
        backend: "codex",
        transport: "websocket",
        outcome: "failed",
        dispatch_origin: "ws_enqueue",
        total_us: 0,
        prepare_us: 0,
        lowering_us: 0,
        serialization_us: 0,
        capture_us: 0,
        pool_wait_us: 0,
        connect_upgrade_us: 0,
        enqueue_us: 0,
        decode_us: 0,
        dispatch_to_first_input_us: None,
        dispatch_to_first_associated_event_us: None,
        dispatch_to_first_text_delta_us: None,
        dispatch_to_first_reasoning_delta_us: None,
        dispatch_to_first_actionable_item_us: None,
        dispatch_to_first_semantic_us: None,
        dispatch_to_terminal_us: None,
        terminal_to_return_us: None,
        request_bytes_total: 0,
        first_input_bytes: 0,
        dispatch_count: 0,
        decode_count: 0,
        connection_state: "unknown",
    };
    let mut submitted = false;
    submit_with(
        CaptureMetadata {
            session_id: &session_id,
            agent_prompt_id: &agent_prompt_id,
            model: "test-model",
            profile: None,
            operation: "inference",
            logical_attempt: Some(1),
            attempt_id: None,
            final_wire_dispatch_index: None,
            repair_reason: "none",
            facts: AttemptFacts::default(),
        },
        timing,
        |_| submitted = true,
    );
    assert!(submitted);
}

/// Manual local overhead probe for the fixed first-seen checks and one JSON
/// serialization; ignored because wall-clock measurements are
/// informational.
#[test]
#[ignore = "manual overhead measurement"]
fn manual_attempt_timing_overhead_probe() {
    const ITERATIONS: u32 = 1_000_000;
    let baseline_started = Instant::now();
    for index in 0..ITERATIONS {
        black_box(index);
    }
    let baseline = baseline_started.elapsed();

    let mut trace = AttemptTrace::selected_for_capture(Backend::Codex, Transport::Websocket, true)
        .expect("capture trace");
    trace.record_dispatch();
    let observed_started = Instant::now();
    for _ in 0..ITERATIONS {
        trace.associated_event();
        trace.text_delta();
        trace.reasoning_delta();
        trace.actionable_item();
    }
    let observed = observed_started.elapsed();
    let timing = trace.finish_with_timing(Outcome::Completed);

    let session_id = "session-timing".parse().expect("session id");
    let agent_prompt_id = "ap-timing".parse().expect("prompt id");
    let serialization_started = Instant::now();
    let bytes = record(
        CaptureMetadata {
            session_id: &session_id,
            agent_prompt_id: &agent_prompt_id,
            model: "test-model",
            profile: Some("test-profile"),
            operation: "inference",
            logical_attempt: Some(1),
            attempt_id: None,
            final_wire_dispatch_index: Some(1),
            repair_reason: "none",
            facts: AttemptFacts::default(),
        },
        timing,
    )
    .expect("bounded timing record");
    let serialization = serialization_started.elapsed();
    eprintln!(
        "attempt_timing state_bytes={} record_bytes={} baseline_ns={} observed_ns={} net_ns_per_event={} serialization_ns={}",
        std::mem::size_of::<AttemptTrace>(),
        bytes.len(),
        baseline.as_nanos(),
        observed.as_nanos(),
        observed.saturating_sub(baseline).as_nanos() / u128::from(ITERATIONS),
        serialization.as_nanos(),
    );
}
