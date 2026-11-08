//! Deliberate, bounded recovery of accepted tool output.

use tau_harness::{AgentId, AgentToolCall, HarnessError, InternalToolHost, ToolOutputTarget};
use tau_proto::{
    BackgroundSupport, CborValue, ToolCallId, ToolCallRef, ToolName, ToolSpec, ToolType,
};

pub(super) fn tool_spec() -> ToolSpec {
    ToolSpec {
        provider_scope: None,
        name: ToolName::new(super::TOOL_OUTPUT_NAME),
        model_visible_name: None,
        description: Some("Read a bounded page of this agent's retained accepted tool output. Use tool_call_id for the first page, then the returned call as reference with next_offset. This deliberate lookup may repeat output; wait never does. Compaction and restart preserve accepted output, not omitted raw process bytes or temporary saved-output files.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "tool_call_id": {"type": "string", "minLength": 1},
                "reference": {
                    "type": "object",
                    "properties": {
                        "declaration": {"type": "string"},
                        "item_index": {"type": "integer", "minimum": 0}
                    },
                    "required": ["declaration", "item_index"],
                    "additionalProperties": false
                },
                "offset": {"type": "integer", "minimum": 0}
            },
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

/// Shape validation happens before dispatch; enforce exclusive selection here.
fn parse(arguments: &CborValue) -> Result<(ToolOutputTarget, usize), String> {
    let call_id = tau_proto::cbor_text_field(arguments, "tool_call_id").map(ToolCallId::from);
    let reference = tau_proto::cbor_field(arguments, "reference")
        .map(|value| {
            value
                .deserialized::<ToolCallRef>()
                .map_err(|error| error.to_string())
        })
        .transpose()?;
    let target = match (call_id, reference) {
        (Some(id), None) => ToolOutputTarget::DisplayId(id),
        (None, Some(reference)) => ToolOutputTarget::Reference(reference),
        _ => return Err("Supply exactly one of tool_call_id and reference".to_owned()),
    };
    let offset = match tau_proto::cbor_field(arguments, "offset") {
        None => 0,
        Some(_) => tau_proto::cbor_int_field(arguments, "offset")
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| "offset must be a nonnegative integer".to_owned())?,
    };
    Ok((target, offset))
}

pub(super) fn handle_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &AgentId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    let result = parse(&call.arguments)
        .and_then(|(target, offset)| host.tool_output_page(conversation_id, target, offset))
        .and_then(|page| CborValue::serialized(&page).map_err(|error| error.to_string()));
    match result {
        Ok(result) => host.finish_tool_with_cbor_result(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            result,
            None,
        ),
        Err(message) => host.finish_tool_with_error(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            message,
            None,
        ),
    }
    Ok(())
}
