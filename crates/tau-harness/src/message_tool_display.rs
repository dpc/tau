//! Shared compact metadata for synchronous and asynchronous message completion.

use tau_proto::{CborValue, PeerMessageReviewReference, ToolUseState, ToolUseStats};

/// Describe the original recipient and text-body size without retaining the
/// body.
///
/// Missing or non-text arguments omit their corresponding metadata. Callers
/// supply progress/error status separately; the default describes success.
pub fn message_tool_display(arguments: &CborValue) -> ToolUseState {
    ToolUseState {
        args: tau_proto::cbor_text_field(arguments, "recipient_id")
            .filter(|recipient| !recipient.trim().is_empty())
            .unwrap_or_default(),
        stats: tau_proto::cbor_text_field(arguments, "message")
            .map(|text| ToolUseStats::for_text(&text))
            .unwrap_or_default(),
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

/// Identify an immutable review occurrence without exposing the review body.
pub fn peer_review_release_display(reference: &PeerMessageReviewReference) -> ToolUseState {
    ToolUseState {
        args: format!(
            "@{} #{}",
            reference.main_agent_id, reference.durable_event_seq
        ),
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}
