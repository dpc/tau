//! Harness-owned semantic work-status tool.

use tau_harness::{AgentOwnedInternalToolCall, HarnessError, InternalToolHost, WorkStatusReport};
use tau_proto::{
    AgentWorkStatusPhase, BackgroundSupport, CborValue, ToolName, ToolSpec, ToolType, ToolUseState,
    ToolUseStatus,
};

/// Build the model-visible status tool contract.
pub(crate) fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("status"),
        model_visible_name: None,
        description: Some(
            "Report meaningful user-level work status to watchers. Use `waiting` when progress is paused pending an expected self-resolving event; use `blocked` when progress requires external intervention. Use an independently informative task name; do not use an opaque identifier or task/ticket number alone. Avoid routine progress or label-only updates; call alongside other independent tools when possible."
                .to_owned(),
        ),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "state": {
                    "type":"string",
                    "enum":["working","done","blocked","waiting"],
                    "description":"`waiting` expects self-resolution; `blocked` requires external intervention."
                },
                "task_name": {"type":"string","description":"Brief, independently informative user-visible task label; do not use opaque identifiers or task/ticket numbers alone."}
            },
            "required": ["state", "task_name"],
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
        Ok((phase, title)) => {
            let result = format!("Status accepted: {} — {title}", phase_name(phase));
            host.finish_tool_with_cbor_result(
                conversation_id,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                CborValue::Text(result),
                Some(display(phase, &title, ToolUseStatus::Success, "ok")),
            );
        }
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
    let mut task_name = None;
    for (key, value) in entries {
        let CborValue::Text(key) = key else {
            return Err("status argument keys must be strings".to_owned());
        };
        match key.as_str() {
            "state" => state = value.as_text().map(ToOwned::to_owned),
            "task_name" => task_name = value.as_text().map(ToOwned::to_owned),
            _ => return Err(format!("unknown status argument `{key}`")),
        }
    }
    let phase = match state.as_deref() {
        Some("working") => AgentWorkStatusPhase::Working,
        Some("done") => AgentWorkStatusPhase::Done,
        Some("blocked") => AgentWorkStatusPhase::Blocked,
        Some("waiting") => AgentWorkStatusPhase::Waiting,
        Some(_) => {
            return Err("status state must be working, done, blocked, or waiting".to_owned());
        }
        None => return Err("status requires state".to_owned()),
    };
    WorkStatusReport::new(
        phase,
        task_name.ok_or_else(|| "status requires task_name".to_owned())?,
    )
}

/// Build the generic tool display that keeps a status report's semantic payload
/// visible while the call is running and after it settles.
pub(crate) fn initial_display(arguments: &CborValue) -> Option<ToolUseState> {
    let report = parse_args(arguments).ok()?;
    Some(display(
        report.phase(),
        report.title(),
        ToolUseStatus::InProgress,
        tau_proto::PROGRESS_INDICATOR_TEXT,
    ))
}

fn display(
    phase: AgentWorkStatusPhase,
    title: &str,
    status: ToolUseStatus,
    status_text: &str,
) -> ToolUseState {
    ToolUseState {
        args: format!("{}: {title}", phase_name(phase)),
        status,
        status_text: status_text.to_owned(),
        ..Default::default()
    }
}

fn phase_name(phase: AgentWorkStatusPhase) -> &'static str {
    match phase {
        AgentWorkStatusPhase::Working => "working",
        AgentWorkStatusPhase::Done => "done",
        AgentWorkStatusPhase::Blocked => "blocked",
        AgentWorkStatusPhase::Waiting => "waiting",
        AgentWorkStatusPhase::Unreported | AgentWorkStatusPhase::Unknown => {
            unreachable!("status parser accepts only model-reportable phases")
        }
    }
}
