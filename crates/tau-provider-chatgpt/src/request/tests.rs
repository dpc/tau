//! Synthetic request oracles independent of account credentials and live
//! models.

use serde_json::json;
use tau_proto::*;

use super::*;

/// Complete portable prompt with no implicit provider capabilities.
fn prompt() -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: "chatgpt-test".parse().expect("prompt"),
        agent_id: AgentId::parse("agent-test").expect("agent"),
        session_id: "session-test".parse().expect("session"),
        system_prompt: "trusted instructions".into(),
        context: PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        local_summary_continuation: Vec::new(),
        hosted_tools: Vec::new(),
        model: "chatgpt-plan/test-model".parse().expect("model"),
        model_params: ModelParams::default(),
        tool_choice: ToolChoice::Auto,
        originator: PromptOriginator::User,
        ctx_id: None,
        compaction: None,
        operation: PromptOperation::Inference,
    }
}

/// Supported local tools are introduced before replay; trusted system messages
/// become developer messages without reserializing admitted opaque items.
#[test]
fn restricted_request_uses_additional_tools_and_preserves_raw_history() {
    let mut prompt = prompt();
    prompt.tools.push(ToolDefinition {
        name: ToolName::new("read_file"),
        model_visible_name: None,
        description: Some("Read a local file".into()),
        tool_type: ToolType::Function,
        parameters: Some(json!({"type":"object","properties":{}})),
        format: None,
    });
    prompt
        .context
        .blocks
        .push(ContextBlock::UserInput(UserInputBlock {
            items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::System,
                content: vec![ContentPart::Text {
                    text: "trusted context".into(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        }));
    let raw = r#"{"type":"reasoning","id":"rs_fixture","encrypted_content":"sealed","summary":[],"extra":1.2300}"#;
    prompt
        .context
        .blocks
        .push(ContextBlock::AssistantResponse(AssistantResponseBlock {
            provider_response_id: Some("old".into()),
            backend: None,
            usage: None,
            output_items: vec![ContextItem::Reasoning(
                OpaqueProviderItem::from_raw_json(raw).expect("opaque"),
            )],
        }));
    let request = lower(
        &prompt,
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    assert!(request.json().get().contains(raw));
    let value: serde_json::Value = serde_json::from_str(request.json().get()).expect("JSON");
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["instructions"], "trusted instructions");
    assert_eq!(value["input"][0]["type"], "additional_tools");
    assert_eq!(value["input"][0]["role"], "developer");
    assert_eq!(value["input"][0]["tools"][0]["name"], "read_file");
    assert_eq!(value["input"][1]["role"], "developer");
    assert_eq!(value["input"].as_array().expect("input").len(), 3);
    for field in [
        "tools",
        "previous_response_id",
        "max_output_tokens",
        "temperature",
        "metadata",
        "background",
        "prompt_cache_retention",
        "prompt",
        "conversation",
        "top_p",
    ] {
        assert!(value.get(field).is_none(), "{field}");
    }
}
