//! Harness-owned runtime self-information tool.

use std::fmt::Write as _;
use std::sync::Arc;

use tau_proto::{AgentWorkStatusPhase, BackgroundSupport, CborValue, ToolName, ToolSpec, ToolType};

use crate::internal_tools::InternalSelfInfo;
use crate::{AgentOwnedInternalToolCall, HarnessError, InternalToolHandler, InternalToolHost};

/// Model-visible name of the harness-owned self-information tool.
pub(crate) const SELF_INFO_TOOL_NAME: &str = "self_info";

/// Stateless harness-owned self-information handler.
struct SelfInfoTool;

impl SelfInfoTool {
    /// Build the model-visible self-information tool contract.
    fn tool_spec() -> ToolSpec {
        ToolSpec {
            name: ToolName::new(SELF_INFO_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Return authoritative runtime identity, model context and compaction configuration, provider quota state when available, and work status for the calling agent."
                    .to_owned(),
            ),
            tool_type: ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(BackgroundSupport::Never),
            examples: Vec::new(),
        }
    }

    /// Validate and serve one self-information call.
    fn handle_tool_call(host: &mut InternalToolHost<'_>, owner: &AgentOwnedInternalToolCall) {
        let call = owner.call();
        let conversation_id = owner.conversation_id().clone();
        let visible_tool_name = owner.visible_tool_name().clone();
        let info = host.self_info(owner);
        match resolve_result(&call.arguments, info.as_ref()) {
            Ok(result) => host.finish_tool_with_cbor_result(
                &conversation_id,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                CborValue::Text(result),
                None,
            ),
            Err(message) => host.finish_tool_with_error(
                &conversation_id,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                message.to_owned(),
                None,
            ),
        }
    }
}

impl InternalToolHandler for SelfInfoTool {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![Self::tool_spec()]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == SELF_INFO_TOOL_NAME
    }

    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &tau_proto::Event,
    ) -> Result<(), HarnessError> {
        let tau_proto::Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((conversation_id, call, visible_tool_name)) = host.internal_started_call(started)
        else {
            return Ok(());
        };
        if call.name.as_str() != SELF_INFO_TOOL_NAME {
            return Ok(());
        }
        let Some(owner) = host.agent_owned_internal_started_call(started) else {
            host.finish_tool_with_error(
                &conversation_id,
                call.id,
                visible_tool_name,
                call.tool_type,
                "configured extensions cannot invoke `self_info`".to_owned(),
                Some(call.arguments),
            );
            return Ok(());
        };
        Self::handle_tool_call(host, &owner);
        Ok(())
    }
}

/// Return the intrinsic handler installed by every harness.
pub(crate) fn handler() -> Arc<dyn InternalToolHandler> {
    Arc::new(SelfInfoTool)
}

/// Resolve the production result contract without publishing its terminal.
fn resolve_result(
    arguments: &CborValue,
    info: Option<&InternalSelfInfo>,
) -> Result<String, &'static str> {
    if arguments != &CborValue::Map(Vec::new()) {
        return Err("self_info arguments must be an empty object");
    }
    info.map(format_headers)
        .ok_or("self_info metadata is unavailable for this call")
}

/// Format the stable line-oriented self-information result.
fn format_headers(info: &InternalSelfInfo) -> String {
    let InternalSelfInfo {
        agent_id,
        session_id,
        session_dir,
        model,
        effort,
        context,
        compaction,
        provider_quota,
        work_status,
    } = info;
    let session_dir = session_dir.as_deref().map_or_else(
        || "(none)".to_owned(),
        |path| escape_header_bytes(path.as_os_str().as_encoded_bytes()),
    );
    let task_name = work_status.title().unwrap_or("(none)");
    let model = model.to_string();
    let mut output = format!(
        "agent_id: {}\nsession_id: {}\nsession_dir: {session_dir}\nmodel: {}\neffort_requested: {}\neffort_effective: {}\nstatus: {}\nstatus_task_name: {task_name}",
        agent_id,
        session_id,
        escape_header_bytes(model.as_bytes()),
        effort.requested,
        effort
            .effective
            .native()
            .map_or_else(|| effort.effective.to_string(), |level| level.to_string()),
        status_name(work_status.phase()),
    );
    append_context(&mut output, context);
    output.push_str("\ncompaction_inference: ");
    output.push_str(&compaction.inference);
    for policy in &compaction.named {
        use tau_config::settings::ContextPolicyPoint;

        let at = match policy.at {
            ContextPolicyPoint::AfterResponse => "after_response",
            ContextPolicyPoint::BeforeInference => "before_inference",
            ContextPolicyPoint::OuterTurnFinished => "outer_turn_finished",
        };
        let statuses = policy.statuses.as_ref().map_or_else(
            || "any".to_owned(),
            |statuses| {
                statuses
                    .iter()
                    .map(|status| status_name(*status))
                    .collect::<Vec<_>>()
                    .join(",")
            },
        );
        let threshold = policy.threshold.map_or_else(
            || "unavailable".to_owned(),
            |threshold| threshold.get().to_string(),
        );
        let _ = write!(
            output,
            "\ncompaction_policy: name={} threshold_tokens={threshold} at={at} statuses={statuses} state={}",
            escape_header_bytes(policy.name.as_bytes()),
            policy.state
        );
    }
    append_provider_quota(&mut output, provider_quota.as_ref());
    output
}

/// Append current model-qualified provider input accounting.
fn append_context(output: &mut String, context: &crate::internal_tools::InternalSelfContext) {
    let input = context
        .input_tokens
        .map_or_else(|| "unavailable".to_owned(), |value| value.get().to_string());
    let cached = context
        .cached_tokens
        .map_or_else(|| "unavailable".to_owned(), |value| value.get().to_string());
    let context_window = context
        .context_window
        .map_or_else(|| "unavailable".to_owned(), |value| value.get().to_string());
    let capacity = context
        .input_token_limit
        .map_or_else(|| "unavailable".to_owned(), |value| value.get().to_string());
    let percent = match (context.input_tokens, context.input_token_limit) {
        (Some(input), Some(capacity)) if capacity != tau_proto::TokenCount::ZERO => {
            let basis_points = u128::from(input.get()) * 10_000 / u128::from(capacity.get());
            format!("{}.{:02}", basis_points / 100, basis_points % 100)
        }
        _ => "unavailable".to_owned(),
    };
    output.push_str(&format!(
        "\ncontext_input_tokens: {input} (latest_provider_reported)\ncontext_cached_tokens: {cached} (latest_provider_reported)\ncontext_window_tokens: {context_window} (provider_advertised_total)\ncontext_input_capacity_tokens: {capacity} (effective_input_limit)\ncontext_input_used_percent: {percent}"
    ));
}

/// Append bounded provider-neutral quota state when the current provider has
/// it.
fn append_provider_quota(
    output: &mut String,
    quota: Option<&crate::internal_tools::InternalSelfProviderQuota>,
) {
    let Some(quota) = quota else {
        output.push_str("\nprovider_quota: unavailable");
        return;
    };
    output.push_str("\nprovider_quota: available");
    let pools = if quota.model_limit_ids.is_empty() {
        "unavailable".to_owned()
    } else {
        quota
            .model_limit_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let binding_age = optional_u64(quota.model_binding_age_seconds);
    output.push_str(&format!(
        "\nprovider_quota_model_binding: pools={pools} observed_age_seconds={binding_age} freshness={}",
        freshness(quota.model_binding_age_seconds)
    ));
    for window in &quota.windows {
        let observed_age = optional_u64(window.observed_age_seconds);
        let remaining = window.remaining_seconds.map_or_else(
            || "unavailable".to_owned(),
            |remaining| remaining.to_string(),
        );
        let reset = optional_u64(window.reset_at_unix_seconds);
        let _ = write!(
            output,
            "\nprovider_quota_window: pool={} window={} used_percent={}.{:02} duration_seconds={} observed_age_seconds={observed_age} freshness={} remaining_seconds={remaining} reset_at_unix_seconds={reset} applies_to_model={}",
            window.limit_id,
            window.window_id,
            window.used_basis_points / 100,
            window.used_basis_points % 100,
            window.window_seconds,
            freshness(window.observed_age_seconds),
            window.applies_to_model
        );
    }
}

/// Format an optional unsigned scalar without inventing a value.
fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

/// Classify observation age using the existing quota freshness boundaries.
fn freshness(age_seconds: Option<u64>) -> &'static str {
    match age_seconds {
        Some(0..=900) => "fresh",
        Some(901..=3600) => "stale",
        Some(_) => "expired",
        None => "unavailable",
    }
}

/// Encode arbitrary bytes as one unambiguous header-line value.
///
/// Printable ASCII stays literal except for doubled backslashes. Every other
/// byte uses `\xNN`, preserving non-UTF-8 paths and preventing control bytes
/// from creating apparent headers.
fn escape_header_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut escaped = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'\\' => escaped.push_str("\\\\"),
            0x20..=0x7e => escaped.push(char::from(*byte)),
            _ => write!(&mut escaped, "\\x{byte:02X}").expect("writing to String cannot fail"),
        }
    }
    escaped
}

fn status_name(status: AgentWorkStatusPhase) -> &'static str {
    match status {
        AgentWorkStatusPhase::Unreported => "unreported",
        AgentWorkStatusPhase::Working => "working",
        AgentWorkStatusPhase::Done => "done",
        AgentWorkStatusPhase::Blocked => "blocked",
        AgentWorkStatusPhase::Waiting => "waiting",
        AgentWorkStatusPhase::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests;
