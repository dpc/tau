//! Bounded owner-private scalar timing captures for finite provider attempts.

use serde_json::json;

use crate::debug_capture_writer::{
    ProviderDebugCapture, ProviderDebugCaptureClass, submit_provider_debug_capture,
};
use crate::private_attempt_trace::AttemptTiming;

/// Maximum serialized size of one provider attempt timing record.
pub const MAX_RECORD_BYTES: usize = 8 * 1024;

/// Stable schema name for offline inspection.
pub const SCHEMA: &str = "tau.provider_attempt_timing";

/// Additive private schema revision with owner-scoped message-read boundaries.
pub const SCHEMA_VERSION: u64 = 2;

/// Initial metric-definition revision.
pub const METRIC_DEFINITION_VERSION: u64 = 1;

/// Adapter-owned metadata that makes one timing sample comparable.
pub struct CaptureMetadata<'a> {
    /// Durable session selected by the existing exact-capture policy.
    pub session_id: &'a tau_proto::SessionId,
    /// Existing prompt attribution for this finite attempt.
    pub agent_prompt_id: &'a tau_proto::AgentPromptId,
    /// Configured provider model identifier.
    pub model: &'a str,
    /// Optional provider profile or route identifier.
    pub profile: Option<&'a str>,
    /// Closed operation kind such as inference or compaction.
    pub operation: &'static str,
    /// One-based scheduler attempt when the adapter exposes it.
    pub logical_attempt: Option<u64>,
    /// Private attempt identifier when already available.
    pub attempt_id: Option<String>,
    /// One-based final wire dispatch when already available.
    pub final_wire_dispatch_index: Option<u64>,
    /// Closed repair reason; `none` means no transparent repair.
    pub repair_reason: &'static str,
    /// Closed adapter-specific scalar workload and terminal facts.
    pub facts: AttemptFacts,
}

/// Closed workload and response facts allowed in timing captures.
#[derive(Default)]
pub struct AttemptFacts {
    /// Configured output-token cap when the adapter has one.
    pub max_output_tokens: Option<u32>,
    /// Whether callable tools were exposed to this attempt.
    pub tool_enabled: Option<bool>,
    /// Whether the accepted response produced a callable tool.
    pub tool_produced: Option<bool>,
    /// Total response bytes observed by the adapter.
    pub response_bytes_received: Option<u64>,
    /// Provider-reported response-local token usage.
    pub usage: Option<ResponseUsage>,
    /// Closed Responses mode when applicable.
    pub response_mode: Option<ResponseMode>,
    /// Whether the provider backend dispatch boundary was reached.
    pub backend_reached: Option<bool>,
}

/// Closed response-local usage projection without session or per-model totals.
#[derive(Clone, Copy)]
pub struct ResponseUsage {
    /// Input tokens sent for this response.
    pub prompt_sent_tokens: u64,
    /// Input tokens reported as cache hits for this response.
    pub prompt_cached_tokens: u64,
    /// Exact response-local cache-read ceiling when available.
    pub prompt_cache_read_ceiling_tokens: Option<u64>,
    /// Output tokens received for this response.
    pub response_received_tokens: u64,
    /// Provider cache-read tokens when available.
    pub cache_read_tokens: Option<u64>,
    /// Provider cache-write tokens when available.
    pub cache_write_tokens: Option<u64>,
    /// Provider cache-miss tokens when available.
    pub cache_miss_tokens: Option<u64>,
    /// Largest response-local cacheable prefix when available.
    pub cacheable_prefix_tokens: Option<u64>,
    /// Estimated response-local prefill tokens avoided when available.
    pub avoided_prefill_tokens: Option<u64>,
    /// Provider-reported response-local cache storage token-time.
    pub storage_token_micros: Option<u64>,
}

impl ResponseUsage {
    /// Project only response-local counters from the provider usage DTO.
    #[must_use]
    pub fn from_provider(usage: &tau_proto::ProviderTokenUsage) -> Self {
        let cache = usage.cache.as_deref().copied().unwrap_or_default();
        Self {
            prompt_sent_tokens: usage.prompt_sent_tokens,
            prompt_cached_tokens: usage.prompt_cached_tokens,
            prompt_cache_read_ceiling_tokens: usage.prompt_cache_read_ceiling_tokens,
            response_received_tokens: usage.response_received_tokens,
            cache_read_tokens: cache.read_tokens,
            cache_write_tokens: cache.write_tokens,
            cache_miss_tokens: cache.miss_tokens,
            cacheable_prefix_tokens: cache.cacheable_prefix_tokens,
            avoided_prefill_tokens: cache.avoided_prefill_tokens,
            storage_token_micros: cache.storage_token_micros.map(|value| value.get()),
        }
    }
}

/// Closed Responses operation mode used for comparison.
#[derive(Clone, Copy)]
pub enum ResponseMode {
    /// Ordinary user-facing inference.
    Ordinary,
    /// Native standalone compaction.
    Compact,
    /// Tau-owned local summary compaction.
    LocalSummary,
}

impl ResponseMode {
    /// Return the stable private schema spelling.
    fn label(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Compact => "compact",
            Self::LocalSummary => "local_summary",
        }
    }
}

/// Serialize and submit one bounded timing record through the existing worker.
///
/// This function performs no compression or I/O. Oversized records are dropped
/// before they can consume shared capture queue capacity.
pub fn submit(metadata: CaptureMetadata<'_>, timing: AttemptTiming) {
    submit_with(metadata, timing, submit_provider_debug_capture);
}

/// Build one current-schema record for deterministic tests and alternate sinks.
pub fn record(metadata: CaptureMetadata<'_>, timing: AttemptTiming) -> Option<Vec<u8>> {
    let value = json!({
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "metric_definition_version": METRIC_DEFINITION_VERSION,
        "producer": {
            "run_id": crate::cache_diagnostic::producer_run_id().map(|id| id.to_hex()),
            "build": crate::cache_diagnostic::producer_build(),
        },
        "clock": {
            "kind": "process_monotonic",
            "unit": "us",
        },
        "attribution": {
            "session_id": metadata.session_id,
            "agent_prompt_id": metadata.agent_prompt_id,
            "attempt_id": metadata.attempt_id,
            "logical_attempt": metadata.logical_attempt,
            "final_wire_dispatch_index": metadata.final_wire_dispatch_index,
        },
        "provider": {
            "backend": timing.backend,
            "transport": timing.transport,
            "model": metadata.model,
            "profile": metadata.profile,
            "operation": metadata.operation,
        },
        "attempt": {
            "outcome": timing.outcome,
            "dispatch_origin": timing.dispatch_origin,
            "dispatch_count": timing.dispatch_count,
            "connection_state": timing.connection_state,
            "repair_reason": metadata.repair_reason,
        },
        "timings_us": {
            "attempt_to_final_dispatch": timing.final_dispatch_us,
            "final_dispatch_to_first_observed_text_message_read":
                timing.text_message_read.map(|(read, _)| read),
            "first_observed_text_message_read_to_owner_dequeue":
                timing.text_message_read.map(|(_, dequeue)| dequeue),
            "final_dispatch_to_first_observed_associated_message_read":
                timing.associated_message_read.map(|(read, _)| read),
            "first_observed_associated_message_read_to_association":
                timing.associated_message_read.map(|(_, association)| association),
            "final_dispatch_to_first_decoded_payload":
                timing.dispatch_to_first_decoded_payload_us,
            "attempt_total": timing.total_us,
            "prepare": timing.prepare_us,
            "lowering": timing.lowering_us,
            "serialization": timing.serialization_us,
            "request_capture": timing.capture_us,
            "pool_wait": timing.pool_wait_us,
            "connect_upgrade": timing.connect_upgrade_us,
            "enqueue_or_send": timing.enqueue_us,
            "decode_total": timing.decode_us,
            "final_dispatch_to_first_owner_dequeued_input":
                timing.dispatch_to_first_input_us,
            "final_dispatch_to_first_associated_event":
                timing.dispatch_to_first_associated_event_us,
            "final_dispatch_to_first_text_delta":
                timing.dispatch_to_first_text_delta_us,
            "final_dispatch_to_first_reasoning_delta":
                timing.dispatch_to_first_reasoning_delta_us,
            "final_dispatch_to_first_actionable_item":
                timing.dispatch_to_first_actionable_item_us,
            "final_dispatch_to_first_semantic":
                timing.dispatch_to_first_semantic_us,
            "final_dispatch_to_terminal": timing.dispatch_to_terminal_us,
            "terminal_to_return": timing.terminal_to_return_us,
        },
        "counts": {
            "request_bytes_total": timing.request_bytes_total,
            "first_owner_dequeued_input_bytes": timing.first_input_bytes,
            "decode_count": timing.decode_count,
        },
        "coverage": {
            "final_dispatch": availability(timing.final_dispatch_us),
            "text_message_read": availability(timing.text_message_read.map(|(read, _)| read)),
            "associated_message_read": availability(timing.associated_message_read.map(|(read, _)| read)),
            "first_decoded_payload": availability(timing.dispatch_to_first_decoded_payload_us),
            "first_owner_dequeued_input": availability(timing.dispatch_to_first_input_us),
            "first_associated_event": availability(timing.dispatch_to_first_associated_event_us),
            "first_text_delta": availability(timing.dispatch_to_first_text_delta_us),
            "first_reasoning_delta": availability(timing.dispatch_to_first_reasoning_delta_us),
            "first_actionable_item": availability(timing.dispatch_to_first_actionable_item_us),
            "first_semantic": availability(timing.dispatch_to_first_semantic_us),
            "terminal": availability(timing.dispatch_to_terminal_us),
            "tail": "not_observed_after_owner_return",
        },
        "facts": {
            "max_output_tokens": metadata.facts.max_output_tokens,
            "tool_enabled": metadata.facts.tool_enabled,
            "tool_produced": metadata.facts.tool_produced,
            "response_bytes_received": metadata.facts.response_bytes_received,
            "usage": metadata.facts.usage.map(|usage| json!({
                "prompt_sent_tokens": usage.prompt_sent_tokens,
                "prompt_cached_tokens": usage.prompt_cached_tokens,
                "prompt_cache_read_ceiling_tokens": usage.prompt_cache_read_ceiling_tokens,
                "response_received_tokens": usage.response_received_tokens,
                "cache_read_tokens": usage.cache_read_tokens,
                "cache_write_tokens": usage.cache_write_tokens,
                "cache_miss_tokens": usage.cache_miss_tokens,
                "cacheable_prefix_tokens": usage.cacheable_prefix_tokens,
                "avoided_prefill_tokens": usage.avoided_prefill_tokens,
                "storage_token_micros": usage.storage_token_micros,
            })),
            "response_mode": metadata.facts.response_mode.map(ResponseMode::label),
            "backend_reached": metadata.facts.backend_reached,
        },
    });
    let bytes = serde_json::to_vec(&value).ok()?;
    (bytes.len() <= MAX_RECORD_BYTES).then_some(bytes)
}

/// Submit through an injected nonblocking sink.
pub fn submit_with(
    metadata: CaptureMetadata<'_>,
    timing: AttemptTiming,
    sink: impl FnOnce(ProviderDebugCapture),
) {
    let session_id = metadata.session_id.clone();
    let agent_prompt_id = metadata.agent_prompt_id.clone();
    let Some(json) = record(metadata, timing) else {
        tracing::warn!(
            target: "tau_provider::provider_attempt_timing",
            "provider attempt timing record exceeds its bound; dropping capture"
        );
        return;
    };
    sink(ProviderDebugCapture::new(
        session_id,
        agent_prompt_id,
        ProviderDebugCaptureClass::ProviderAttemptTiming,
        json,
    ));
}

/// Explain nullable first-milestone values without converting absence to zero.
fn availability(value: Option<u64>) -> &'static str {
    if value.is_some() {
        "observed"
    } else {
        "not_observed"
    }
}

#[cfg(test)]
#[path = "provider_attempt_timing/tests.rs"]
mod tests;
