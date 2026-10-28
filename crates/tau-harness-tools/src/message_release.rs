//! Narrow approval of immutable retained peer reviews by session main.

use tau_harness::{AgentId, AgentToolCall, HarnessError, InternalToolHost};
use tau_proto::{
    BackgroundSupport, CborValue, PeerMessageReviewReference, ToolName, ToolSpec, ToolType,
};

/// Model-visible operation accepts a retained occurrence reference, never
/// replacement message facts.
pub(super) fn tool_spec() -> ToolSpec {
    ToolSpec {
        provider_scope: None,
        name: ToolName::new(super::MESSAGE_RELEASE_TOOL_NAME),
        model_visible_name: None,
        description: Some("Approve immutable delivery of one retained peer review. Only this session's canonical main may call this. Copy the reference from the review; sender, intended worker, body, and notices cannot be edited. A retained worker receipt makes retries return already delivered. Success does not promise inference or a reply.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "object",
                    "properties": {
                        "main_agent_id": {"type": "string", "description": "Canonical main owning the review journal."},
                        "durable_event_seq": {"type": "integer", "minimum": 0, "description": "Accepted review occurrence sequence."}
                    },
                    "required": ["main_agent_id", "durable_event_seq"],
                    "additionalProperties": false
                }
            },
            "required": ["reference"],
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

/// Parses only the occurrence identity; core schema validation rejects unknown
/// fields before this production handler runs.
fn parse_reference(arguments: &CborValue) -> Result<PeerMessageReviewReference, String> {
    let reference = tau_proto::cbor_field(arguments, "reference")
        .ok_or_else(|| "reference is required".to_owned())?;
    let main = tau_proto::cbor_text_field(reference, "main_agent_id")
        .ok_or_else(|| "reference.main_agent_id must be a string".to_owned())?;
    let seq = tau_proto::cbor_int_field(reference, "durable_event_seq")
        .and_then(|seq| u64::try_from(seq).ok())
        .ok_or_else(|| "reference.durable_event_seq must be a nonnegative u64".to_owned())?;
    Ok(PeerMessageReviewReference {
        main_agent_id: AgentId::parse(main).map_err(|error| error.to_string())?,
        durable_event_seq: seq,
    })
}

/// Tracks the original tool before calling the host, allowing both immediate
/// commit completion and deferred/intercepted publication.
pub(super) fn handle_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &AgentId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    let result = parse_reference(&call.arguments).and_then(|source| {
        host.release_peer_review(
            conversation_id,
            source,
            call.id.clone(),
            visible_tool_name.clone(),
            call.tool_type,
        )
    });
    match result {
        Ok(false) => {}
        Ok(true) => host.finish_tool_with_cbor_result(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            CborValue::Text(
                "Already delivered: retained worker receipt exists; no new message appended"
                    .to_owned(),
            ),
            None,
        ),
        Err(error) => host.finish_tool_with_display_error(
            conversation_id,
            call.id.clone(),
            visible_tool_name,
            call.tool_type,
            error,
            None,
            None,
        ),
    }
    Ok(())
}
