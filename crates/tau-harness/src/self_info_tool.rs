//! Harness-owned runtime self-information tool.

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
                "Return authoritative runtime identity, session, model route, and work-status metadata for the calling agent."
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
        work_status,
    } = info;
    let session_dir = session_dir.as_deref().map_or_else(
        || "(none)".to_owned(),
        |path| escape_header_bytes(path.as_os_str().as_encoded_bytes()),
    );
    let task_name = work_status.title().unwrap_or("(none)");
    let model = model.to_string();
    format!(
        "agent_id: {}\nsession_id: {}\nsession_dir: {session_dir}\nmodel: {}\neffort: {}\nstatus: {}\nstatus_task_name: {task_name}",
        agent_id,
        session_id,
        escape_header_bytes(model.as_bytes()),
        effort.as_str(),
        status_name(work_status.phase()),
    )
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
