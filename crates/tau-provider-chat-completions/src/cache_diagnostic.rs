//! Bounded Chat Completions observations, never accounting or transport
//! authority.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tau_provider::cache_diagnostic::{CacheDiagnostics, DiagnosticId, Reservation};
use tau_provider::debug_capture_writer::{ProviderDebugCapture, ProviderDebugCaptureClass};

use crate::capture_correlation::CaptureCorrelation;
use crate::{AttemptConfig, AttemptModel, AttemptOutcome, CacheUsageCompat, ChatRequest};

#[cfg(test)]
pub(crate) mod tests;

/// Private finite-attempt identity and fixed-cardinality raw observations.
pub(super) struct CacheAttempt {
    /// Generated identity shared with exact captures, never sent upstream.
    id: DiagnosticId,
    /// Process-local producer identity.
    run_id: DiagnosticId,
    /// Existing typed session attribution.
    session_id: tau_proto::SessionId,
    /// Existing typed agent attribution.
    agent_id: tau_proto::AgentId,
    /// Existing typed prompt/operation identity.
    prompt_id: tau_proto::AgentPromptId,
    /// Backend operation represented by this finite invocation.
    operation: tau_proto::PromptOperation,
    /// Real owner-supplied finite attempt.
    provider_attempt: tau_proto::ProviderAttempt,
    /// Metadata selection independent of exact captures.
    enabled: bool,
    /// Monotonic diagnostic start time.
    started: Instant,
    /// At most one observed send per finite invocation.
    dispatched: AtomicBool,
    /// Latest observed usage member, projected before canonical normalization.
    usage: Mutex<Option<Value>>,
}

impl CacheAttempt {
    /// Select persistable inference or local-summary activity.
    pub(super) fn new(
        prompt: &tau_proto::AgentPromptCreated,
        persistable: bool,
        policy: CacheDiagnostics,
        provider_attempt: tau_proto::ProviderAttempt,
    ) -> Option<Self> {
        if !persistable {
            return None;
        }
        Some(Self {
            id: DiagnosticId::random()?,
            run_id: tau_provider::cache_diagnostic::producer_run_id()?,
            session_id: prompt.session_id.clone(),
            agent_id: prompt.agent_id.clone(),
            prompt_id: prompt.agent_prompt_id.clone(),
            operation: prompt.operation,
            provider_attempt,
            enabled: policy == CacheDiagnostics::Metadata,
            started: Instant::now(),
            dispatched: AtomicBool::new(false),
            usage: Mutex::new(None),
        })
    }

    /// Read only the actual dispatch observation, including for exact captures.
    pub(super) fn correlation(&self) -> CaptureCorrelation {
        CaptureCorrelation {
            attempt_id: Some(self.id),
            wire_dispatch_index: self.dispatched.load(Ordering::Relaxed).then_some(1),
        }
    }

    /// Replace, never merge or sum, each observed allowlisted usage snapshot.
    /// The attempt owns this projection so a later stream failure cannot lose
    /// it.
    pub(super) fn record_usage(&self, usage: &Value, compat: CacheUsageCompat) {
        if self.enabled
            && let Ok(mut stored) = self.usage.lock()
        {
            *stored = Some(reported_usage(Some(usage), compat));
        }
    }

    /// Construct closed metadata only after bounded shared admission.
    fn common(&self, reservation: &Reservation, kind: &'static str) -> Value {
        json!({
            "schema": "tau.cache_diagnostic", "schema_version": 0,
            "producer_build": tau_provider::cache_diagnostic::producer_build(),
            "producer_run_id": self.run_id, "record_seq": reservation.sequence(),
            "record_kind": kind, "attempt_id": self.id,
            "session_id": self.session_id, "agent_id": self.agent_id,
            "agent_prompt_id": self.prompt_id, "operation_id": self.prompt_id,
            "operation": operation_label(self.operation),
            "logical_attempt": self.provider_attempt.get(),
            "harness_provider_attempt": self.provider_attempt,
            "recorded_at_unix_micros": micros(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()),
            "elapsed_us_since_attempt_start": micros(self.started.elapsed()),
            "dropped_records_total": reservation.dropped_records_total(),
            "capabilities": {"exact_request": true, "exact_response": true,
                "raw_attribution": false, "chain_facts": true},
            "malformed_fields": [], "omitted_identity_fields": [],
        })
    }

    /// Observe the existing pre-send callback; do not inspect opaque extra_body
    /// controls or serialize the request again for byte sizing.
    pub(super) fn dispatch(
        &self,
        prompt: &tau_proto::AgentPromptCreated,
        config: &AttemptConfig,
        model: &AttemptModel,
        body: &ChatRequest,
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
                "backend": "chat_completions", "backend_mode": "standard", "transport": "http_sse",
                "configured_model": model, "effective_model": model,
                "omitted_identity_fields": if model.is_none() { vec!["configured_model", "effective_model"] } else { vec![] },
                "wire_dispatch_index": 1, "request_bytes": bytes,
                "context_item_count": prompt.context.flatten_iter().count(),
                "input_item_count": body.messages.len(), "tool_count": body.tools.len(),
                "tool_choice": body.tool_choice, "reasoning_selector": body.reasoning_effort,
                "service_tier": null,
                "cache_mode": config.compat.prompt_cache.map(|p| p.mode.wire()),
                "cache_ttl_seconds": config.compat.prompt_cache.map(|p| match p.ttl {
                    crate::PromptCacheTtl::Minutes30 => 1800_u64,
                }),
                "request_form": "full", "previous_response_present": false,
                "anchor_validation": "not_applicable", "connection_epoch": null,
                "connection_state": "not_applicable", "repair_reason": "none", "repair_used": false,
            }),
        );
        self.submit(record, reservation);
    }

    /// Emit one normally returned attempt outcome without retaining error
    /// prose.
    pub(super) fn finish(&self, outcome: &AttemptOutcome) {
        if !self.enabled {
            return;
        }
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let index = self.correlation().wire_dispatch_index;
        let (success, canceled, semantic, failure, retry_class) = match outcome {
            AttemptOutcome::Completed(success) => {
                (true, false, success.has_timed_semantic_output(), None, None)
            }
            AttemptOutcome::Canceled { progress, .. } => (
                false,
                true,
                *progress == crate::SemanticProgress::Parsed,
                None,
                None,
            ),
            AttemptOutcome::Retryable {
                decision, progress, ..
            } => (
                false,
                false,
                *progress == crate::SemanticProgress::Parsed,
                None,
                Some(decision.class),
            ),
            AttemptOutcome::Terminal(failure) => (
                false,
                false,
                failure.progress == crate::SemanticProgress::Parsed,
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
                .unwrap_or_else(|| reported_usage(None, CacheUsageCompat::None)),
        );
        merge(
            &mut record,
            json!({
                "backend": "chat_completions", "transport": "http_sse",
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

    /// Reuse the shared bounded best-effort compression and IPC writer.
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

/// Map the closed prompt operation to the diagnostic wire label.
fn operation_label(operation: tau_proto::PromptOperation) -> &'static str {
    match operation {
        tau_proto::PromptOperation::Inference => "inference",
        tau_proto::PromptOperation::StandaloneCompaction => "standalone_compaction",
    }
}

/// Closed retry labels, never provider prose.
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

/// Project only the exact route's supported counter shape. Absence is not zero;
/// malformed values yield null and bounded closed field labels.
fn reported_usage(usage: Option<&Value>, compat: CacheUsageCompat) -> Value {
    let mut malformed = Vec::new();
    if usage.is_some_and(|v| !v.is_null() && !v.is_object()) {
        malformed.push("reported_usage");
    }
    for field in ["completion_tokens_details", "prompt_tokens_details"] {
        if (field != "prompt_tokens_details" || compat == CacheUsageCompat::OpenAi)
            && usage
                .and_then(|v| v.get(field))
                .is_some_and(|v| !v.is_null() && !v.is_object())
        {
            malformed.push(field);
        }
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
    let input = read(&["prompt_tokens"], "input_tokens", &mut malformed);
    let output = read(&["completion_tokens"], "output_tokens", &mut malformed);
    let reasoning = read(
        &["completion_tokens_details", "reasoning_tokens"],
        "reasoning_output_tokens",
        &mut malformed,
    );
    let (cached, write, miss) = match compat {
        CacheUsageCompat::None => (None, None, None),
        CacheUsageCompat::OpenAi => (
            read(
                &["prompt_tokens_details", "cached_tokens"],
                "read_tokens",
                &mut malformed,
            ),
            read(
                &["prompt_tokens_details", "cache_write_tokens"],
                "write_tokens",
                &mut malformed,
            )
            .or_else(|| read(&["cache_write_tokens"], "write_tokens", &mut malformed)),
            None,
        ),
        CacheUsageCompat::DeepSeek => (
            read(&["prompt_cache_hit_tokens"], "read_tokens", &mut malformed),
            None,
            read(&["prompt_cache_miss_tokens"], "miss_tokens", &mut malformed),
        ),
    };
    json!({
        "reported_usage": {
            "input_tokens": input, "read_tokens": cached, "write_tokens": write,
            "output_tokens": output, "reasoning_output_tokens": reasoning,
            "miss_tokens": miss, "storage_token_micros": null,
        },
        "malformed_fields": malformed,
        "attribution_status": if usage.is_some() { "unsupported_shape" } else { "absent" },
        "attribution_total_check": "not_checkable", "attribution": [], "omitted_entries": 0,
    })
}

/// Omit identities over 128 bytes or containing the dispatched credential.
fn bounded_model<'a>(model: &'a str, credential: &str) -> Option<&'a str> {
    (model.len() <= 128 && (credential.is_empty() || !model.contains(credential))).then_some(model)
}

/// Merge locally constructed closed-schema fields only.
fn merge(record: &mut Value, fields: Value) {
    record
        .as_object_mut()
        .expect("record object")
        .extend(fields.as_object().expect("fields object").clone());
}

/// Saturating local observation time, never an upstream receipt timestamp.
fn micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}
