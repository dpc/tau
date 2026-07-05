use tau_proto::{MessageItem, PromptOriginator, ProviderStopReason};

use super::*;

#[test]
fn prompt_stdin_role_uses_startup_role_or_default() {
    assert_eq!(prompt_stdin_role(Some("specialist")), "specialist");
    assert_eq!(prompt_stdin_role(None), DEFAULT_AGENT_ROLE);
}
fn user_update(spid: &str, text: &str, thinking: Option<&str>) -> ProviderResponseUpdated {
    let mut deltas = Vec::new();
    if let Some(thinking) = thinking.filter(|thinking| !thinking.is_empty()) {
        deltas.push(tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 0,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: thinking.to_owned(),
        });
    }
    if !text.is_empty() {
        deltas.push(tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: text.to_owned(),
            phase: None,
        });
    }
    ProviderResponseUpdated {
        agent_prompt_id: spid.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas,
        compaction: None,
        status: None,
        progress: None,
        originator: PromptOriginator::User,
    }
}

fn user_status_clear_update(spid: &str) -> ProviderResponseUpdated {
    ProviderResponseUpdated {
        agent_prompt_id: spid.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
        }),
        progress: None,
        originator: PromptOriginator::User,
    }
}

fn assistant_finished(
    spid: &str,
    text: &str,
    stop_reason: ProviderStopReason,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: spid.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason,
        originator: PromptOriginator::User,
        error: None,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// The one-shot client ignores streaming updates for display but keeps the
/// appended streaming deltas so finished turns can print reasoning blocks
/// and the final answer only once the agent is done.
#[test]
fn one_shot_output_waits_through_tool_calls_and_keeps_final_snapshots() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-tool", "", Some("plan v1")));
    output.capture_update(&user_update("sp-tool", "", Some(" final")));

    assert!(!output.capture_finished(&ProviderResponseFinished {
        agent_prompt_id: "sp-tool".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        originator: PromptOriginator::User,
        output_items: Vec::new(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));

    output.capture_update(&user_update(
        "sp-final",
        "streamed answer",
        Some("answer plan"),
    ));
    assert!(output.capture_finished(&assistant_finished(
        "sp-final",
        "final answer",
        ProviderStopReason::EndTurn,
    )));

    assert_eq!(output.thinking_blocks, vec!["plan v1 final", "answer plan"]);
    assert_eq!(output.final_response.as_deref(), Some("final answer"));
}

/// Some provider paths may have accumulated streaming text but no final
/// assistant message item; fall back to accumulated deltas rather than
/// printing nothing.
#[test]
fn one_shot_output_falls_back_to_latest_streaming_text() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-final", "partial", None));
    output.capture_update(&user_update("sp-final", "complete", None));

    assert!(output.capture_finished(&ProviderResponseFinished {
        agent_prompt_id: "sp-final".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        originator: PromptOriginator::User,
        output_items: Vec::new(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));

    assert_eq!(output.final_response.as_deref(), Some("partialcomplete"));
}

/// Provider retry/status resets must clear stale failed-attempt fallback text
/// so one-shot mode does not print reasoning or answer text from replaced work.
#[test]
fn one_shot_output_status_clear_resets_streaming_fallback() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-final", "bad", Some("bad plan")));
    output.capture_update(&user_status_clear_update("sp-final"));
    output.capture_update(&user_update("sp-final", "good", Some("good plan")));

    assert!(output.capture_finished(&ProviderResponseFinished {
        agent_prompt_id: "sp-final".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        originator: PromptOriginator::User,
        output_items: Vec::new(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));

    assert_eq!(output.thinking_blocks, vec!["good plan"]);
    assert_eq!(output.final_response.as_deref(), Some("good"));
}
