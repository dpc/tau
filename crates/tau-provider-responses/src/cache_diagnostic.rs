//! Public Responses scalar evidence, never transport or accounting authority.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tau_provider::cache_diagnostic::{CacheDiagnostics, DiagnosticId, Reservation};
use tau_provider::debug_capture_writer::{ProviderDebugCapture, ProviderDebugCaptureClass};

use crate::{AttemptConfig, AttemptModel, AttemptOutcome, RequestBody, Transport};

#[cfg(test)]
pub(crate) mod tests;

/// Private correlation and bounded observations for one finite invocation.
pub(super) struct CacheAttempt {
    /// Random identity shared by exact captures, never sent upstream.
    pub(super) id: DiagnosticId,
    /// Process-local producer identity.
    run_id: DiagnosticId,
    /// Existing typed session attribution.
    session_id: tau_proto::SessionId,
    /// Existing typed agent attribution.
    agent_id: tau_proto::AgentId,
    /// Existing typed prompt and operation identity.
    prompt_id: tau_proto::AgentPromptId,
    /// Owner-supplied attempt, not an inferred logical-attempt ordinal.
    provider_attempt: Option<tau_proto::ProviderAttempt>,
    /// Metadata selection independent of exact capture selection.
    enabled: bool,
    /// Monotonic diagnostic start time.
    started: Instant,
    /// At most one attempted dispatch; not provider receipt.
    dispatched: AtomicBool,
    /// Fixed allowlisted raw counters, never raw events or normalized usage.
    usage: Mutex<Option<Value>>,
}

impl CacheAttempt {
    /// Select only persistable ordinary inference; local-summary compaction
    /// retains its existing exact captures without acquiring scalar coverage.
    pub(super) fn new(
        prompt: &tau_proto::AgentPromptCreated,
        persistable: bool,
        policy: CacheDiagnostics,
        provider_attempt: Option<tau_proto::ProviderAttempt>,
    ) -> Option<Self> {
        if !persistable || prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
            return None;
        }
        Some(Self {
            id: DiagnosticId::random()?,
            run_id: tau_provider::cache_diagnostic::producer_run_id()?,
            session_id: prompt.session_id.clone(),
            agent_id: prompt.agent_id.clone(),
            prompt_id: prompt.agent_prompt_id.clone(),
            provider_attempt,
            enabled: policy == CacheDiagnostics::Metadata,
            started: Instant::now(),
            dispatched: AtomicBool::new(false),
            usage: Mutex::new(None),
        })
    }

    /// Return an actual dispatch index only after the existing observation.
    pub(super) fn dispatch_index(&self) -> Option<u64> {
        self.dispatched.load(Ordering::Relaxed).then_some(1)
    }

    /// Observe the already-parsed terminal usage before canonical
    /// normalization.
    pub(super) fn record_usage(&self, usage: Option<&Value>) {
        if self.enabled
            && let Ok(mut stored) = self.usage.lock()
        {
            *stored = Some(reported_usage(usage));
        }
    }

    /// Construct only fixed-cardinality metadata after acquiring admission.
    fn common(&self, reservation: &Reservation, kind: &'static str) -> Value {
        json!({
            "schema": "tau.cache_diagnostic", "schema_version": 0,
            "producer_build": tau_provider::cache_diagnostic::producer_build(),
            "producer_run_id": self.run_id, "record_seq": reservation.sequence(),
            "record_kind": kind, "attempt_id": self.id,
            "session_id": self.session_id, "agent_id": self.agent_id,
            "agent_prompt_id": self.prompt_id, "operation_id": self.prompt_id,
            "operation": "inference", "logical_attempt": null,
            "harness_provider_attempt": self.provider_attempt,
            "recorded_at_unix_micros": micros(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()),
            "elapsed_us_since_attempt_start": micros(self.started.elapsed()),
            "dropped_records_total": reservation.dropped_records_total(),
            "capabilities": {"exact_request": true, "exact_response": true,
                "raw_attribution": false, "chain_facts": true},
            "malformed_fields": [], "omitted_identity_fields": [],
        })
    }

    /// Record attempted send at the existing boundary, without local repair,
    /// connection reuse, or extra serialization for byte sizing.
    pub(super) fn dispatch(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        body: &RequestBody,
        bytes: usize,
    ) {
        if self.dispatched.swap(true, Ordering::Relaxed) || !self.enabled {
            return;
        }
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let mut record = self.common(&reservation, "dispatch");
        let model = bounded_model(model.id.as_ref(), &config.api_key);
        merge(
            &mut record,
            json!({
                "backend": "public_responses", "backend_mode": "standard",
                "transport": transport(config),
                "configured_model": model, "effective_model": model,
                "omitted_identity_fields": if model.is_none() { vec!["configured_model", "effective_model"] } else { vec![] },
                "wire_dispatch_index": 1, "request_bytes": bytes,
                "context_item_count": prompt.context.flatten_iter().count(),
                "input_item_count": body.input.len(), "tool_count": prompt.tools.len(),
                "tool_choice": prompt.tool_choice,
                "reasoning_selector": body.reasoning.effort,
                "service_tier": null,
                "cache_mode": config.prompt_cache.map(|p| p.mode.wire()),
            "cache_ttl_seconds": config.prompt_cache.map(|p| match p.ttl {
                crate::PromptCacheTtl::Minutes30 => 1800_u64,
            }),
                "request_form": "full", "previous_response_present": false,
                "anchor_validation": "not_applicable",
                "connection_epoch": null,
                "connection_state": match config.transport {
                Transport::Sse => "not_applicable", Transport::Websocket => "new",
                },
            "repair_reason": "none", "repair_used": false,
            }),
        );
        self.submit(record, reservation);
    }

    /// Emit once for a normally returned outcome, without retaining error prose
    /// or changing the extension's terminal and retry policy.
    pub(super) fn finish(&self, config: &AttemptConfig, outcome: &AttemptOutcome) {
        if !self.enabled {
            return;
        }
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let index = self.dispatch_index();
        let (success, canceled, semantic, failure, retry_class) = match outcome {
            AttemptOutcome::Completed(success) => {
                (true, false, success.has_timed_semantic_output, None, None)
            }
            AttemptOutcome::Canceled { progress } => {
                (false, true, progress.has_timed_semantic_output, None, None)
            }
            AttemptOutcome::Retryable { decision, progress } => (
                false,
                false,
                progress.has_timed_semantic_output,
                None,
                Some(decision.class),
            ),
            AttemptOutcome::Terminal(failure) => (
                false,
                false,
                failure.progress.has_timed_semantic_output,
                failure.failure_kind,
                None,
            ),
        };
        let mut record = self.common(&reservation, "attempt_end");
        merge(
            &mut record,
            self.usage
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_else(|| reported_usage(None)),
        );
        merge(
            &mut record,
            json!({
                "backend": "public_responses", "transport": transport(config),
                "dispatch_count": u64::from(index.is_some()),
                "successful_dispatch_index": if success { index } else { None },
                "outcome": if canceled { "canceled" } else if success { "success" }
                    else if index.is_none() { "pre_dispatch_failure" } else { "error" },
                "failure_class": failure, "retry_class": retry_class.map(retry_label),
                "semantic_progress": semantic, "repair_used": false,
                "reconnect_count": 0, "chain_strip_count": 0,
                "actual_model": null, "model_revision": null, "actual_service_tier": null,
                "reported_eligibility": null,
            }),
        );
        self.submit(record, reservation);
    }

    /// Reuse bounded shared admission and off-path compression/IPC.
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

/// Closed adapter-owned labels, not retry display prose.
fn retry_label(class: tau_provider::retry_policy::RetryClass) -> &'static str {
    use tau_provider::retry_policy::RetryClass;
    match class {
        RetryClass::Transport => "transport",
        RetryClass::Overload => "overload",
        RetryClass::Throttle => "throttle",
        RetryClass::UsageWindow => "usage_window",
        RetryClass::Account => "account",
        RetryClass::Auth => "auth",
        RetryClass::Unknown => "unknown",
    }
}

/// Preserve absence and malformed values as null with closed reason codes.
fn reported_usage(usage: Option<&Value>) -> Value {
    let mut malformed = Vec::new();
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
        let value = path.iter().try_fold(usage?, |v, key| v.get(key))?;
        if value.is_null() {
            return None;
        }
        let counter = value.as_u64();
        if counter.is_none() {
            malformed.push(code);
        }
        counter
    };
    let counters = json!({
        "input_tokens": read(&["input_tokens"], "input_tokens", &mut malformed),
        "read_tokens": read(&["input_tokens_details", "cached_tokens"], "read_tokens", &mut malformed),
        "write_tokens": read(&["input_tokens_details", "cache_write_tokens"], "write_tokens", &mut malformed)
            .or_else(|| read(&["cache_write_tokens"], "write_tokens", &mut malformed)),
        "output_tokens": read(&["output_tokens"], "output_tokens", &mut malformed),
        "reasoning_output_tokens": null, "miss_tokens": null, "storage_token_micros": null,
    });
    json!({
        "reported_usage": counters, "malformed_fields": malformed,
        "attribution_status": if usage.is_some() { "unsupported_shape" } else { "absent" },
        "attribution_total_check": "not_checkable", "attribution": [], "omitted_entries": 0,
    })
}

/// Omit identities containing the dispatched credential or exceeding 128 bytes.
fn bounded_model<'a>(model: &'a str, credential: &str) -> Option<&'a str> {
    (model.len() <= 128 && (credential.is_empty() || !model.contains(credential))).then_some(model)
}

/// Stable adapter-owned transport label.
fn transport(config: &AttemptConfig) -> &'static str {
    match config.transport {
        Transport::Sse => "http_sse",
        Transport::Websocket => "websocket",
    }
}

/// Merge only locally constructed closed-schema fields.
fn merge(record: &mut Value, fields: Value) {
    record
        .as_object_mut()
        .expect("record object")
        .extend(fields.as_object().expect("fields object").clone());
}

/// Saturating local observation duration, never a receipt timestamp.
fn micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}
