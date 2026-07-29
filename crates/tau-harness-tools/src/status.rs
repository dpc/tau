//! Harness-owned semantic work-status tool.

use tau_harness::{AgentOwnedInternalToolCall, HarnessError, InternalToolHost, WorkStatusReport};
use tau_proto::{AgentWorkStatusPhase, BackgroundSupport, CborValue, ToolName, ToolSpec, ToolType};

/// Build the model-visible status tool contract.
pub(crate) fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("status"),
        model_visible_name: None,
        description: Some("Report this agent's current task status to its watchers.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "state": {"type":"string","enum":["working","done","blocked"]},
                "title": {"type":"string","description":"Canonical one-line UTF-8 title (160 bytes maximum)."}
            },
            "required": ["state", "title"],
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

/// Validate and apply one status call to its owning agent.
pub(crate) fn handle_tool_call(
    host: &mut InternalToolHost<'_>,
    owner: &AgentOwnedInternalToolCall,
) -> Result<(), HarnessError> {
    let conversation_id = owner.conversation_id();
    let call = owner.call();
    let visible_tool_name = owner.visible_tool_name().clone();
    let parsed = parse_tool_args(&call.arguments);
    match parsed.and_then(|report| {
        let phase = report.phase();
        let title = report.title().to_owned();
        host.report_work_status(owner, report)
            .map_err(|message| (message, None))
            .map(|_| (phase, title))
    }) {
        Ok((phase, title)) => host.finish_tool_with_result(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            format!("Status accepted: {} — {title}", phase_name(phase)),
            None,
        ),
        Err((message, details)) => host.finish_tool_with_error(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            message,
            details,
        ),
    }
    Ok(())
}

/// Parse a tool call while redacting rejected arguments from its error.
pub(super) fn parse_tool_args(
    arguments: &CborValue,
) -> Result<WorkStatusReport, (String, Option<CborValue>)> {
    parse_args(arguments).map_err(|message| (message, None))
}

/// Parse and canonicalize the closed status argument contract.
pub(crate) fn parse_args(arguments: &CborValue) -> Result<WorkStatusReport, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("status arguments must be an object".to_owned());
    };
    let mut state = None;
    let mut title = None;
    for (key, value) in entries {
        let CborValue::Text(key) = key else {
            return Err("status argument keys must be strings".to_owned());
        };
        match key.as_str() {
            "state" => state = value.as_text().map(ToOwned::to_owned),
            "title" => title = value.as_text().map(ToOwned::to_owned),
            _ => return Err(format!("unknown status argument `{key}`")),
        }
    }
    let phase = match state.as_deref() {
        Some("working") => AgentWorkStatusPhase::Working,
        Some("done") => AgentWorkStatusPhase::Done,
        Some("blocked") => AgentWorkStatusPhase::Blocked,
        Some(_) => return Err("status state must be working, done, or blocked".to_owned()),
        None => return Err("status requires state".to_owned()),
    };
    WorkStatusReport::new(
        phase,
        title.ok_or_else(|| "status requires title".to_owned())?,
    )
}

fn phase_name(phase: AgentWorkStatusPhase) -> &'static str {
    match phase {
        AgentWorkStatusPhase::Working => "working",
        AgentWorkStatusPhase::Done => "done",
        AgentWorkStatusPhase::Blocked => "blocked",
        AgentWorkStatusPhase::Unreported | AgentWorkStatusPhase::Unknown => {
            unreachable!("status parser accepts only model-reportable phases")
        }
    }
}
