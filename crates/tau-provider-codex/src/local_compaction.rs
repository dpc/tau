//! Tau-owned Codex summary lowering and private terminal validation.

use std::sync::Arc;

use tau_provider::cache_diagnostic::CacheDiagnostics;
use tau_provider::local_summary_compaction;

use crate::cache_diagnostic::{CacheAttempt, CompactEvidence};
use crate::common::{LlmError, OutputItemAccumulator};
use crate::{
    AttemptOperation, CodexError, CodexRuntime, CompactOutcome, LogicalAttempt, Prompt,
    ProviderAttemptContext, ResolvedConfig, ResponseMode, StreamState, StreamUpdate, TurnAbort,
    private_trace,
};

impl CodexRuntime {
    /// Execute one local-summary request for the original standalone
    /// transaction.
    ///
    /// No logical retry or transparent repair follows this attempt, including
    /// when it follows a definitive native-route rejection. All semantic output
    /// stays private until the complete narrative passes validation.
    pub fn local_compact_numbered(
        &self,
        agent_prompt_id: &str,
        logical_attempt: LogicalAttempt,
        config: &ResolvedConfig,
        request: &Prompt<'_>,
        abort: &mut impl TurnAbort,
    ) -> CompactOutcome {
        if abort.is_aborted() {
            return CompactOutcome::Canceled {
                backend_reached: false,
            };
        }
        let mut context = request.context.clone();
        if let Err(error) = local_summary_compaction::replace_trailing_trigger(&mut context) {
            return invalid_output(error, false);
        }
        let summary_request = Prompt {
            system_prompt: request.system_prompt,
            context: &context,
            tools: request.tools,
            hosted_tools: request.hosted_tools,
            params: request.params,
            tool_choice: tau_proto::ToolChoice::None,
            compaction: None,
            originator: request.originator,
            session_id: request.session_id,
            agent_id: request.agent_id,
            debug_provider_requests: request.debug_provider_requests,
        };
        // Start a fresh chain with the exact ordinary prefix, not an anchor
        // representing the native attempt or a discarded summary.
        if let Err(error) = self.ws_pool.invalidate(config.wire(), request) {
            return CompactOutcome::Terminal {
                error: CodexError(error.into_llm_error()),
                backend_reached: false,
            };
        }
        let mut attempt = ProviderAttemptContext::new(AttemptOperation::Compact, logical_attempt);
        let metadata_enabled = self
            .cache_diagnostics
            .get_or_init(Default::default)
            .get(&config.wire().profile_namespace)
            .copied()
            .unwrap_or_default()
            == CacheDiagnostics::Metadata;
        let diagnostic = CacheAttempt::new(
            agent_prompt_id,
            &summary_request,
            logical_attempt.get(),
            metadata_enabled,
        )
        .map(CacheAttempt::standalone_compaction)
        .map(Arc::new);
        attempt.correlation().diagnostic = diagnostic.clone();
        let mut backend_reached = false;
        let mut trace = private_trace::AttemptTrace::selected(
            private_trace::Backend::Codex,
            private_trace::Transport::Websocket,
        );
        let result = self.stream(
            agent_prompt_id,
            config.wire(),
            &summary_request,
            ResponseMode::LocalSummary,
            attempt.correlation(),
            abort,
            &mut |update| {
                if matches!(update, StreamUpdate::Dispatched(_)) {
                    backend_reached = true;
                }
            },
            &mut trace,
        );
        let mut evidence = CompactEvidence::default();
        if metadata_enabled && diagnostic.is_some() {
            evidence.observe(config.wire(), &result);
        }
        let outcome = if abort.is_aborted() || matches!(result, Err(LlmError::Canceled)) {
            CompactOutcome::Canceled { backend_reached }
        } else {
            match result {
                Ok(dispatch) => {
                    let usage = dispatch.state.usage();
                    match validate_narrative(dispatch.state) {
                        Ok(item) => CompactOutcome::Finished {
                            output_items: vec![item],
                            usage,
                        },
                        Err(error) => invalid_output(error, backend_reached),
                    }
                }
                Err(error) => CompactOutcome::Terminal {
                    error: CodexError(error),
                    backend_reached,
                },
            }
        };
        if let Some(diagnostic) = diagnostic {
            diagnostic.finish_compact(&outcome, evidence, attempt.snapshot(), config.wire());
        }
        if let Some(trace) = trace {
            trace.finish(match &outcome {
                CompactOutcome::Finished { .. } => private_trace::Outcome::Completed,
                CompactOutcome::Canceled { .. } => private_trace::Outcome::Canceled,
                _ => private_trace::Outcome::Failed,
            });
        }
        outcome
    }
}

/// Reject raw tool/unknown slots too, including incomplete calls that ordinary
/// inference intentionally drops. Reasoning is bounded and discarded, never
/// incorporated in the accepted narrative.
fn validate_narrative(state: StreamState) -> Result<tau_proto::ContextItem, &'static str> {
    let limit = tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES;
    let mut reasoning_bytes = state.thinking.as_ref().map_or(0, String::len);
    if limit < reasoning_bytes {
        return Err("summary compactor reasoning exceeds its byte limit");
    }
    let mut narrative = None;
    for item in state.output_items {
        match item {
            OutputItemAccumulator::Empty => {}
            OutputItemAccumulator::Reasoning(item) => {
                reasoning_bytes = reasoning_bytes.saturating_add(item.raw_json().len());
                if limit < reasoning_bytes {
                    return Err("summary compactor reasoning exceeds its byte limit");
                }
            }
            OutputItemAccumulator::Message(message) if narrative.is_none() => {
                narrative = Some(message.text);
            }
            _ => return Err("summary compactor returned unsupported output"),
        }
    }
    let Some(narrative) = narrative else {
        return Err("summary compactor did not return exactly one message");
    };
    if narrative.trim().is_empty() || narrative.len() > limit {
        return Err("summary compactor output is empty or exceeds its byte limit");
    }
    Ok(tau_proto::ContextItem::LocalCompactionNarrative(
        tau_proto::LocalCompactionNarrativeItem { narrative },
    ))
}

/// Construct a content-free terminal validation failure.
fn invalid_output(error: &'static str, backend_reached: bool) -> CompactOutcome {
    CompactOutcome::Terminal {
        error: CodexError(LlmError::InvalidResponse(error.to_owned())),
        backend_reached,
    }
}

/// Reject other semantic output before ordinary projection can drop it or
/// overwrite its slot. Provider errors retain ordinary error-first semantics.
pub(crate) fn validate_event(event: &serde_json::Value) -> Result<(), LlmError> {
    let kind = event["type"].as_str().unwrap_or("");
    let valid = match kind {
        "error" | "response.failed" | "response.incomplete" => true,
        "response.output_item.added" | "response.output_item.done" => {
            valid_summary_item(&event["item"])
        }
        "response.content_part.added" | "response.content_part.done" => {
            event["part"]["type"].as_str() == Some("output_text")
        }
        "response.completed" | "response.done" => {
            event["response"].get("output").is_none_or(|items| {
                items.as_array().is_some_and(|items| {
                    items.iter().all(valid_summary_item)
                        && items
                            .iter()
                            .filter(|item| item["type"] == "reasoning")
                            .map(reasoning_item_bytes)
                            .fold(0_usize, usize::saturating_add)
                            <= tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES
                })
            })
        }
        "response.created" | "response.in_progress" | "response.queued" => true,
        kind if kind.starts_with("response.output_text.")
            || kind.starts_with("response.reasoning_") =>
        {
            true
        }
        kind => !kind.starts_with("response."),
    };
    if valid {
        Ok(())
    } else {
        Err(LlmError::InvalidResponse(
            "summary compactor returned unsupported output".to_owned(),
        ))
    }
}

/// Only ordinary assistant text and discarded provider reasoning may enter a
/// summary stream; tool calls, refusals, and unknown output are terminal.
fn valid_summary_item(item: &serde_json::Value) -> bool {
    match item["type"].as_str() {
        Some("reasoning") => {
            reasoning_item_bytes(item) <= tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES
        }
        Some("message") if item["role"].as_str() == Some("assistant") => {
            item.get("content").is_none_or(|content| {
                content.as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .all(|part| part["type"].as_str() == Some("output_text"))
                })
            })
        }
        _ => false,
    }
}

/// Count the whole discarded reasoning item, including opaque/encrypted fields,
/// independently of token usage. Failure to encode cannot authorize acceptance.
fn reasoning_item_bytes(item: &serde_json::Value) -> usize {
    serde_json::to_vec(item).map_or(usize::MAX, |bytes| bytes.len())
}

#[cfg(test)]
mod tests;
