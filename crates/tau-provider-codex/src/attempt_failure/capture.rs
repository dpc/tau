use serde_json::{Value, json};
use tau_provider::debug_capture_writer as path_tau_provider_debug_capture_writer;

use super::redacted_detail::contains_token_shape;
use super::{AttemptCaptureSnapshot, AttemptFailureEvidence, TransportFailureKind, WsTermination};
use crate::{Prompt, SemanticProgress};

const MAX_CAPTURE_BYTES: usize = 65_536;

/// Outcome of applying the exact capture byte bound.
pub(super) enum BoundedRecord {
    /// Serialized record ready for the shared writer.
    Ready {
        /// Pretty-printed JSON within the exact private-capture byte bound.
        serialized: Vec<u8>,
    },
    /// Record remained oversized after dropping structural shape.
    Oversized,
}

/// Validate and copy one bounded identifier after rejecting secret-shaped data.
pub(super) fn validated_identifier(
    candidate: Option<&str>,
    access_token: &str,
    account_id: Option<&str>,
    truncated: &mut bool,
) -> Option<String> {
    let candidate = candidate?;
    let has_secret = [Some(access_token), account_id]
        .into_iter()
        .flatten()
        .any(|secret| !secret.is_empty() && candidate.contains(secret));
    let token_shaped = contains_token_shape(candidate);
    let valid = !candidate.is_empty()
        && candidate.len() < 1_025
        && candidate.chars().count() < 257
        && !has_secret
        && !token_shaped
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte));
    if !valid {
        *truncated = true;
    }
    valid.then(|| candidate.to_owned())
}

fn lengths(value: Option<&str>) -> Value {
    json!({
        "present": value.is_some(),
        "utf8_bytes": value.map_or(0, |value| u64::try_from(value.len()).unwrap_or(u64::MAX)),
        "unicode_scalars": value.map_or(0, |value| u64::try_from(value.chars().count()).unwrap_or(u64::MAX)),
    })
}

/// Inputs required to assemble one schema-v1 attempt-failure record.
pub(crate) struct CaptureInput<'a> {
    /// Operation performed by this finite attempt.
    pub(crate) operation: crate::attempt_context::AttemptOperation,
    /// Agent prompt correlated with the failed finite attempt.
    pub(crate) agent_prompt_id: &'a str,
    /// Prompt-owned session and capture eligibility.
    pub(crate) request: &'a Prompt<'a>,
    /// Closed scheduler classification and trusted retry hint.
    pub(crate) decision: &'a tau_provider::retry_policy::RetryDecision,
    /// Whether parser-accepted semantic output preceded failure.
    pub(crate) progress: SemanticProgress,
    /// Exact final wire dispatch, absent when provider work never started.
    pub(crate) correlation: AttemptCaptureSnapshot,
    /// Saturating cumulative transport bytes across transparent repair.
    pub(crate) response_bytes_received: u64,
    /// Opaque parser/transport observation, when available.
    pub(crate) evidence: Option<&'a AttemptFailureEvidence>,
    /// Loaded access token used only for exact secret rejection.
    pub(crate) access_token: &'a str,
    /// Loaded account identifier used only for exact secret rejection.
    pub(crate) account_id: Option<&'a str>,
}

/// Submit one bounded failure record through the shared best-effort writer.
pub(crate) fn submit_capture(input: CaptureInput<'_>) {
    submit_capture_with(
        input,
        tau_provider::debug_capture_writer::submit_provider_debug_capture,
    );
}

/// Assemble and submit a capture through an injected writer for focused tests.
pub(crate) fn submit_capture_with(
    input: CaptureInput<'_>,
    submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
) {
    if !input.request.debug_provider_requests {
        return;
    }
    let Ok(agent_prompt_id) = tau_proto::AgentPromptId::parse(input.agent_prompt_id) else {
        return;
    };
    let mut identifiers_truncated = false;
    let (provider, transport, established, shape_truncated) = match input.evidence {
        Some(AttemptFailureEvidence::Provider(provider)) => {
            let event_type = validated_identifier(
                provider.event_type.as_deref(),
                input.access_token,
                input.account_id,
                &mut identifiers_truncated,
            );
            let code = validated_identifier(
                provider.canonical_code.as_deref(),
                input.access_token,
                input.account_id,
                &mut identifiers_truncated,
            );
            let request_id = validated_identifier(
                provider.request_id.as_deref(),
                input.access_token,
                input.account_id,
                &mut identifiers_truncated,
            );
            let response_id = validated_identifier(
                provider.response_id.as_deref(),
                input.access_token,
                input.account_id,
                &mut identifiers_truncated,
            );
            identifiers_truncated |= provider.identifiers_truncated;
            (
                Some(json!({
                    "terminal_event_type": event_type,
                    "canonical_error_code": code,
                    "provider_request_id": request_id,
                    "provider_response_id": response_id,
                    "message": provider.message.as_ref().map_or_else(
                        || lengths(None),
                        |message| json!({
                            "present": true,
                            "utf8_bytes": message.inspected_utf8_bytes,
                            "unicode_scalars": message.inspected_unicode_scalars,
                        }),
                    ),
                    "terminal_event_shape": provider.shape,
                })),
                None,
                true,
                provider.shape_truncated,
            )
        }
        Some(AttemptFailureEvidence::Transport {
            phase,
            established,
            kind,
            request_id,
            identifiers_truncated: observed_identifiers_truncated,
        }) => {
            identifiers_truncated |= *observed_identifiers_truncated;
            let (code, reason, clean_eof, frame_bytes) = match kind {
                TransportFailureKind::WebSocketTermination(WsTermination::CleanEof) => {
                    (None, None, true, None)
                }
                TransportFailureKind::WebSocketTermination(WsTermination::CloseFrame {
                    code,
                    reason,
                }) => (*code, reason.as_deref(), false, None),
                TransportFailureKind::Frame(frame) => (
                    None,
                    None,
                    false,
                    Some(u64::try_from(frame.response_bytes()).unwrap_or(u64::MAX)),
                ),
                _ => (None, None, false, None),
            };
            let request_id = validated_identifier(
                request_id.as_deref(),
                input.access_token,
                input.account_id,
                &mut identifiers_truncated,
            );
            (
                request_id.map(|request_id| {
                    json!({
                        "terminal_event_type": Value::Null,
                        "canonical_error_code": Value::Null,
                        "provider_request_id": request_id,
                        "provider_response_id": Value::Null,
                        "message": lengths(None),
                        "terminal_event_shape": Value::Null,
                    })
                }),
                Some(json!({
                    "phase": phase.label(),
                    "kind": kind.label(),
                    "ws_close_code": code,
                    "ws_close_reason": lengths(reason),
                    "clean_eof": clean_eof,
                    "frame_bytes": frame_bytes,
                })),
                *established,
                false,
            )
        }
        None => (None, None, false, false),
    };
    let mut record = json!({
        "schema_version": 1,
        "capture_kind": "provider_attempt_failure",
        "operation": input.operation.label(),
        "session_id": input.request.session_id,
        "agent_prompt_id": agent_prompt_id,
        "logical_attempt": input.correlation.logical_attempt(),
        "wire_dispatch_index": input
            .evidence
            .filter(|evidence| evidence.failure_was_dispatched())
            .and_then(|_| {
                (0 < input.correlation.wire_dispatches())
                    .then_some(input.correlation.wire_dispatches())
            }),
        "backend": {
            "kind": "responses",
            "transport_intent": "websocket",
            "transport_established": established,
        },
        "outcome": "retry_scheduled",
        "classification": {
            "category": retry_class_label(input.decision.class),
            "retry_after_secs": input.decision.retry_after.map(|duration| duration.as_secs()),
        },
        "wire": {
            "wire_dispatches": input.correlation.wire_dispatches(),
            "repair_used": input.correlation.repair_used(),
            "response_bytes_received": input.response_bytes_received,
            "semantic_progress": match input.progress {
                SemanticProgress::None => "none",
                SemanticProgress::Parsed => "parsed",
            },
        },
        "provider": provider,
        "transport": transport,
        "truncation": {
            "total": false,
            "shape": shape_truncated,
            "identifiers": identifiers_truncated,
        },
    });
    if let Some(attempt_id) = input.correlation.attempt_id {
        record["attempt_id"] = json!(attempt_id);
    }
    let serialized = match serialize_bounded_record(record) {
        Ok(BoundedRecord::Ready { serialized }) => serialized,
        Ok(BoundedRecord::Oversized) => {
            tracing::warn!(
                target: crate::LOG_TARGET,
                "provider failure capture exceeded bounded size"
            );
            return;
        }
        Err(_) => return,
    };
    submit(
        path_tau_provider_debug_capture_writer::ProviderDebugCapture::new(
            input.request.session_id.clone(),
            agent_prompt_id,
            path_tau_provider_debug_capture_writer::ProviderDebugCaptureClass::ResponsesAttemptFailure,
            serialized,
        ),
    );
}

/// Serialize one record and return an oversized outcome when dropping shape
/// still does not satisfy the exact private-capture byte limit.
pub(super) fn serialize_bounded_record(mut record: Value) -> serde_json::Result<BoundedRecord> {
    let mut serialized = serde_json::to_vec_pretty(&record)?;
    if MAX_CAPTURE_BYTES < serialized.len() {
        record["provider"]["terminal_event_shape"] = Value::Null;
        record["truncation"]["total"] = Value::Bool(true);
        serialized = serde_json::to_vec_pretty(&record)?;
    }
    Ok(if serialized.len() <= MAX_CAPTURE_BYTES {
        BoundedRecord::Ready { serialized }
    } else {
        BoundedRecord::Oversized
    })
}

fn retry_class_label(class: tau_provider::retry_policy::RetryClass) -> &'static str {
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
