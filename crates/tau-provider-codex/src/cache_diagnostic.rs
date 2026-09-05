//! Codex-owned scalar projections. No body, error prose, route or provider ID
//! crosses this boundary. Unknown attribution/eligibility stays unavailable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tau_provider::cache_diagnostic::{DiagnosticId, Reservation};
use tau_provider::debug_capture_writer::{ProviderDebugCapture, ProviderDebugCaptureClass};

#[cfg(test)]
pub(crate) mod tests;

/// Capture-local lifetime of one ordinary finite inference attempt.
pub(crate) struct CacheAttempt {
    /// Random identity shared with exact captures even when metadata is off.
    pub(crate) id: DiagnosticId,
    /// Process identity never used for upstream routing.
    run_id: DiagnosticId,
    /// Existing typed session attribution.
    session_id: tau_proto::SessionId,
    /// Existing typed agent attribution.
    agent_id: tau_proto::AgentId,
    /// Existing typed prompt attribution, also the operation identity.
    prompt_id: tau_proto::AgentPromptId,
    /// Existing finite-attempt ordinal, not a harness retry counter.
    logical_attempt: u64,
    /// Metadata selection independent of existing exact captures.
    enabled: bool,
    /// Monotonic attempt entry observation.
    started: Instant,
    /// Attempted dispatches, incremented immediately before enqueue even if it
    /// fails; this is not successful transport acceptance or provider receipt.
    dispatched: AtomicU64,
}

impl std::fmt::Debug for CacheAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheAttempt(<private>)")
    }
}

impl CacheAttempt {
    /// Start capture correlation only for explicitly persistable activity.
    pub(crate) fn new(
        prompt_id: &str,
        request: &crate::Prompt<'_>,
        logical_attempt: u64,
        enabled: bool,
    ) -> Option<Self> {
        if !request.debug_provider_requests {
            return None;
        }
        Some(Self {
            id: DiagnosticId::random()?,
            run_id: tau_provider::cache_diagnostic::producer_run_id()?,
            session_id: request.session_id.clone(),
            agent_id: request.agent_id.clone(),
            prompt_id: tau_proto::AgentPromptId::parse(prompt_id).ok()?,
            logical_attempt,
            enabled,
            started: Instant::now(),
            dispatched: AtomicU64::new(0),
        })
    }

    /// Return attempted dispatches, not checkout/lowering or accepted-send
    /// counts.
    pub(crate) fn dispatch_count(&self) -> u64 {
        self.dispatched.load(Ordering::Relaxed)
    }

    /// Allocate common metadata after reserving capacity.
    fn common(&self, reservation: &Reservation, kind: &'static str) -> Value {
        json!({
            "schema": "tau.cache_diagnostic", "schema_version": 0,
            "producer_build": tau_provider::cache_diagnostic::producer_build(),
            "record_kind": kind, "producer_run_id": self.run_id,
            "record_seq": reservation.sequence(), "attempt_id": self.id,
            "session_id": self.session_id, "agent_id": self.agent_id,
            "agent_prompt_id": self.prompt_id, "operation": "inference",
            "operation_id": self.prompt_id, "logical_attempt": self.logical_attempt,
            "harness_provider_attempt": null,
            "recorded_at_unix_micros": micros(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()),
            "elapsed_us_since_attempt_start": micros(self.started.elapsed()),
            "dropped_records_total": reservation.dropped_records_total(),
            "capabilities": {"exact_request": true, "exact_response": true,
                "raw_attribution": false, "chain_facts": true},
            "malformed_fields": [], "omitted_identity_fields": []
        })
    }

    /// Observe attempted enqueue even when enqueue or metadata admission fails.
    pub(crate) fn dispatch(&self, mut fields: Value, bytes: usize) {
        let index = self.dispatched.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.enabled {
            return;
        }
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let mut record = self.common(&reservation, "dispatch");
        fields["wire_dispatch_index"] = index.into();
        fields["request_bytes"] = (bytes as u64).into();
        merge(&mut record, fields);
        self.submit(record, reservation);
    }

    /// Submit one normal finite-attempt exit; process death can omit it.
    pub(crate) fn finish(
        &self,
        config: &crate::responses::ResponsesConfig,
        result: &Result<crate::StreamDispatchResult, crate::common::LlmError>,
        snapshot: crate::attempt_failure::AttemptCaptureSnapshot,
        canceled: bool,
    ) {
        if !self.enabled {
            return;
        }
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let count = self.dispatch_count();
        let canceled = canceled || matches!(result, Err(crate::common::LlmError::Canceled));
        let success = result.is_ok() && !canceled;
        let outcome = if canceled {
            "canceled"
        } else if success {
            "success"
        } else if count == 0 {
            "pre_dispatch_failure"
        } else {
            "error"
        };
        let mut record = self.common(&reservation, "attempt_end");
        let event = result
            .as_ref()
            .ok()
            .and_then(|r| r.debug_capture.terminal_event.as_ref());
        let response = event.and_then(|e| e.get("response")).or(event);
        let usage = response.and_then(|r| r.get("usage"));
        let mut malformed = Vec::new();
        let mut omitted = Vec::new();
        let reported = reported_usage(usage, &mut malformed);
        if response
            .and_then(|r| r.get("model"))
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            malformed.push("actual_model");
        }
        let actual_model = identity(
            response
                .and_then(|r| r.get("model"))
                .and_then(Value::as_str),
            "actual_model",
            config,
            &mut omitted,
        );
        let tier = response.and_then(|r| r.get("service_tier"));
        let actual_tier = tier
            .and_then(Value::as_str)
            .filter(|s| matches!(*s, "auto" | "default" | "flex" | "priority" | "scale"));
        if tier.is_some_and(|v| !v.is_null()) && actual_tier.is_none() {
            malformed.push("actual_service_tier");
        }
        merge(
            &mut record,
            json!({
                "dispatch_count": count, "successful_dispatch_index": success.then_some(count).filter(|n| *n > 0),
                "outcome": outcome,
            "failure_class": result.as_ref().err().and_then(|e| e.failure_kind()),
                "semantic_progress": snapshot.semantic_progress() == crate::SemanticProgress::Parsed,
                "repair_used": snapshot.repair_used(),
                "reconnect_count": u64::from(snapshot.repair_used()),
                "chain_strip_count": null,
                "actual_model": actual_model, "model_revision": null,
                "actual_service_tier": actual_tier,
                "reported_usage": reported, "reported_eligibility": null,
                "attribution_status": if usage.is_some() { "unsupported_shape" } else { "absent" },
                "attribution_total_check": "not_checkable", "attribution": [], "omitted_entries": 0,
                "malformed_fields": malformed, "omitted_identity_fields": omitted
            }),
        );
        self.submit(record, reservation);
    }

    /// Serialize only the closed bounded projection and reuse the opaque
    /// worker.
    fn submit(&self, record: Value, reservation: Reservation) {
        #[cfg(test)]
        if tests::observe(&record) {
            return;
        }
        let Ok(json) = serde_json::to_vec(&record) else {
            return;
        };
        tau_provider::debug_capture_writer::submit_cache_diagnostic(
            ProviderDebugCapture::new(
                self.session_id.clone(),
                self.prompt_id.clone(),
                ProviderDebugCaptureClass::CacheDiagnostic,
                json,
            ),
            reservation,
        );
    }
}

/// Prepared closed dispatch facts; constructed without prompt traversal.
pub(crate) struct DispatchEvidence {
    /// One inference attempt, shared only within the backend.
    pub(crate) attempt: std::sync::Arc<CacheAttempt>,
    /// Closed/scalar adapter projection with no provider body.
    pub(crate) fields: Value,
}

impl DispatchEvidence {
    /// Record immediately before handing the final serialized text to
    /// transport.
    pub(crate) fn dispatched(&self, bytes: usize) {
        self.attempt.dispatch(self.fields.clone(), bytes);
    }
}

/// Project already-lowered shape without cloning or hashing provider input.
pub(crate) fn dispatch_fields(
    config: &crate::responses::ResponsesConfig,
    request: &crate::Prompt<'_>,
    shape: RequestShape,
    correlation: &crate::attempt_failure::DispatchCorrelation,
) -> Value {
    let mut omitted = Vec::new();
    let model = identity(
        Some(&config.model_id),
        "effective_model",
        config,
        &mut omitted,
    );
    json!({
        "backend": "responses", "transport": "websocket",
        "backend_mode": if config.mode.is_lite_compatibility() { "lite_compatibility" } else { "standard" },
        "configured_model": null, "effective_model": model,
        "reasoning_selector": shape.reasoning_selector,
        "tool_choice": request.tool_choice, "service_tier": shape.service_tier,
        "cache_mode": null, "cache_ttl_seconds": null,
        "context_item_count": null, "input_item_count": shape.input_count, "tool_count": request.tools.len(),
        "request_form": if correlation.repair_used { "repair_full" } else if shape.previous_response_present { "anchored_suffix" } else { "full" },
        "previous_response_present": shape.previous_response_present,
        "anchor_validation": if shape.previous_response_present { "matched" } else if shape.anchor_was_available { "mismatched" } else { "not_applicable" },
        "connection_epoch": shape.connection_epoch,
        "connection_state": correlation.connection_state,
        "repair_reason": correlation.repair_reason,
        "repair_used": correlation.repair_used,
        "omitted_identity_fields": omitted
    })
}

/// Scalar facts borrowed from the final lowered envelope and its socket owner.
pub(crate) struct RequestShape {
    /// Already-materialized wire input array length.
    pub(crate) input_count: usize,
    /// Presence only; the provider's actual anchor identifier stays private.
    pub(crate) previous_response_present: bool,
    /// Whether lowering evaluated a socket-local anchor or prewarm baseline.
    pub(crate) anchor_was_available: bool,
    /// Optional monotonic socket instance number.
    pub(crate) connection_epoch: Option<u64>,
    /// Exact adapter-owned closed literal selected by reasoning lowering.
    pub(crate) reasoning_selector: Option<&'static str>,
    /// Exact adapter-owned closed literal selected by tier lowering.
    pub(crate) service_tier: Option<&'static str>,
}

/// Omit oversized and known-secret-containing identity values in their
/// entirety.
fn identity<'a>(
    value: Option<&'a str>,
    code: &'static str,
    config: &crate::responses::ResponsesConfig,
    omitted: &mut Vec<&'static str>,
) -> Option<&'a str> {
    let value = value?;
    if value.len() > 128
        || [Some(config.api_key.as_str()), config.account_id.as_deref()]
            .into_iter()
            .flatten()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    {
        omitted.push(code);
        None
    } else {
        Some(value)
    }
}

/// Preserve only adapter-established raw counters, before canonical
/// normalization.
fn reported_usage(usage: Option<&Value>, malformed: &mut Vec<&'static str>) -> Value {
    if usage.is_some_and(|v| !v.is_null() && !v.is_object()) {
        malformed.push("reported_usage");
    }
    if usage
        .and_then(|v| v.get("input_tokens_details"))
        .is_some_and(|v| !v.is_null() && !v.is_object())
    {
        malformed.push("input_tokens_details");
    }
    let read = |path: &[&str], code, malformed: &mut Vec<&'static str>| {
        let value = path.iter().try_fold(usage?, |value, key| value.get(key))?;
        if value.is_null() {
            return None;
        }
        let counter = value.as_u64();
        if counter.is_none() {
            malformed.push(code);
        }
        counter
    };
    json!({
        "input_tokens": read(&["input_tokens"], "input_tokens", malformed),
        "read_tokens": read(&["input_tokens_details", "cached_tokens"], "read_tokens", malformed),
        "write_tokens": read(&["input_tokens_details", "cache_write_tokens"], "write_tokens", malformed)
            .or_else(|| read(&["cache_write_tokens"], "write_tokens", malformed)),
        "output_tokens": read(&["output_tokens"], "output_tokens", malformed),
        "reasoning_output_tokens": null, "miss_tokens": null, "storage_token_micros": null
    })
}

/// Merge only adapter-constructed fixed field names.
fn merge(record: &mut Value, fields: Value) {
    record
        .as_object_mut()
        .expect("common object")
        .extend(fields.as_object().expect("fields object").clone());
}

/// Saturating diagnostic microseconds, never a wire timestamp.
fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
