//! Exact structural validation for current provider-attempt timing captures.

use serde_json::Value;

/// One required producer field and its structural predicate.
type FieldCheck<'a> = (&'a str, fn(&Value) -> bool);

/// Validates the complete current schema-v1, metric-definition-v1 record.
pub(super) fn current(value: &Value) -> bool {
    object(
        value,
        &[
            ("schema", |v| literal(v, &["tau.provider_attempt_timing"])),
            ("schema_version", |v| literal_u64(v, 1)),
            ("metric_definition_version", |v| literal_u64(v, 1)),
            ("producer", producer),
            ("clock", clock),
            ("attribution", attribution),
            ("provider", provider),
            ("attempt", attempt),
            ("timings_us", timings),
            ("counts", counts),
            ("coverage", coverage),
            ("facts", facts),
        ],
    )
}

/// Validates producer identity without retaining either private value.
fn producer(value: &Value) -> bool {
    object(
        value,
        &[("run_id", nullable_string), ("build", Value::is_string)],
    )
}

/// Validates the fixed monotonic clock definition.
fn clock(value: &Value) -> bool {
    object(
        value,
        &[
            ("kind", |v| literal(v, &["process_monotonic"])),
            ("unit", |v| literal(v, &["us"])),
        ],
    )
}

/// Validates prompt and attempt attribution used only for inventory placement.
fn attribution(value: &Value) -> bool {
    object(
        value,
        &[
            ("session_id", Value::is_string),
            ("agent_prompt_id", Value::is_string),
            ("attempt_id", nullable_string),
            ("logical_attempt", nullable_u64),
            ("final_wire_dispatch_index", nullable_u64),
        ],
    )
}

/// Validates closed provider comparison dimensions without retaining them.
fn provider(value: &Value) -> bool {
    object(
        value,
        &[
            ("backend", |v| {
                literal(v, &["chat_completions", "public_responses", "codex"])
            }),
            ("transport", |v| {
                literal(v, &["http_sse", "websocket", "http_unary"])
            }),
            ("model", Value::is_string),
            ("profile", nullable_string),
            ("operation", |v| literal(v, &["inference", "compact"])),
        ],
    )
}

/// Validates closed attempt outcome and connection facts.
fn attempt(value: &Value) -> bool {
    object(
        value,
        &[
            ("outcome", |v| {
                literal(v, &["completed", "retryable", "canceled", "failed"])
            }),
            ("dispatch_origin", |v| {
                literal(v, &["ws_enqueue", "http_send_start"])
            }),
            ("dispatch_count", Value::is_u64),
            ("connection_state", |v| {
                literal(v, &["unknown", "reused", "new", "replaced"])
            }),
            ("repair_reason", |v| {
                literal(v, &["none", "stale_response", "other_typed"])
            }),
        ],
    )
}

/// Validates all required durations and nullable final-dispatch milestones.
fn timings(value: &Value) -> bool {
    object(
        value,
        &[
            ("attempt_total", Value::is_u64),
            ("prepare", Value::is_u64),
            ("lowering", Value::is_u64),
            ("serialization", Value::is_u64),
            ("request_capture", Value::is_u64),
            ("pool_wait", Value::is_u64),
            ("connect_upgrade", Value::is_u64),
            ("enqueue_or_send", Value::is_u64),
            ("decode_total", Value::is_u64),
            ("final_dispatch_to_first_owner_dequeued_input", nullable_u64),
            ("final_dispatch_to_first_associated_event", nullable_u64),
            ("final_dispatch_to_first_text_delta", nullable_u64),
            ("final_dispatch_to_first_reasoning_delta", nullable_u64),
            ("final_dispatch_to_first_actionable_item", nullable_u64),
            ("final_dispatch_to_first_semantic", nullable_u64),
            ("final_dispatch_to_terminal", nullable_u64),
            ("terminal_to_return", nullable_u64),
        ],
    )
}

/// Validates bounded request/response counters.
fn counts(value: &Value) -> bool {
    object(
        value,
        &[
            ("request_bytes_total", Value::is_u64),
            ("first_owner_dequeued_input_bytes", Value::is_u64),
            ("decode_count", Value::is_u64),
        ],
    )
}

/// Validates explicit milestone availability and the fixed tail boundary.
fn coverage(value: &Value) -> bool {
    object(
        value,
        &[
            ("first_owner_dequeued_input", availability),
            ("first_associated_event", availability),
            ("first_text_delta", availability),
            ("first_reasoning_delta", availability),
            ("first_actionable_item", availability),
            ("first_semantic", availability),
            ("terminal", availability),
            ("tail", |v| literal(v, &["not_observed_after_owner_return"])),
        ],
    )
}

/// Validates closed optional comparison facts without projecting their values.
fn facts(value: &Value) -> bool {
    object(
        value,
        &[
            ("max_output_tokens", nullable_u64),
            ("tool_enabled", nullable_bool),
            ("tool_produced", nullable_bool),
            ("response_bytes_received", nullable_u64),
            ("usage", |v| v.is_null() || usage(v)),
            ("response_mode", |v| {
                v.is_null() || literal(v, &["ordinary", "compact", "local_summary"])
            }),
            ("backend_reached", nullable_bool),
        ],
    )
}

/// Validates the complete response-local usage projection.
fn usage(value: &Value) -> bool {
    object(
        value,
        &[
            ("prompt_sent_tokens", Value::is_u64),
            ("prompt_cached_tokens", Value::is_u64),
            ("prompt_cache_read_ceiling_tokens", nullable_u64),
            ("response_received_tokens", Value::is_u64),
            ("cache_read_tokens", nullable_u64),
            ("cache_write_tokens", nullable_u64),
            ("cache_miss_tokens", nullable_u64),
            ("cacheable_prefix_tokens", nullable_u64),
            ("avoided_prefill_tokens", nullable_u64),
            ("storage_token_micros", nullable_u64),
        ],
    )
}

/// Requires exactly the named fields with their current producer-owned types.
fn object(value: &Value, required: &[FieldCheck<'_>]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == required.len()
            && required
                .iter()
                .all(|(name, check)| object.get(*name).is_some_and(check))
    })
}

/// Checks one fixed string from a closed producer-owned vocabulary.
fn literal(value: &Value, allowed: &[&str]) -> bool {
    value.as_str().is_some_and(|text| allowed.contains(&text))
}

/// Checks one fixed unsigned schema revision.
fn literal_u64(value: &Value, expected: u64) -> bool {
    value.as_u64() == Some(expected)
}

/// Requires an unsigned number or explicit null.
fn nullable_u64(value: &Value) -> bool {
    value.is_null() || value.is_u64()
}

/// Requires a string or explicit null.
fn nullable_string(value: &Value) -> bool {
    value.is_null() || value.is_string()
}

/// Requires a boolean or explicit null.
fn nullable_bool(value: &Value) -> bool {
    value.is_null() || value.is_boolean()
}

/// Checks whether a nullable milestone was observed.
fn availability(value: &Value) -> bool {
    literal(value, &["observed", "not_observed"])
}
