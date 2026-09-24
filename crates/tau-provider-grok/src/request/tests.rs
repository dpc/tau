//! Synthetic documented xAI request fixtures, not captured subscription
//! traffic.

mod partial_terminals;

use serde_json::{Value, json};
use tau_proto::*;

use super::*;

/// Minimal destination-projected prompt without inferred model capabilities.
fn prompt() -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: "grok-test".parse().expect("prompt id"),
        agent_id: AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test".parse().expect("session id"),
        system_prompt: "test system".into(),
        context: PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        local_summary_continuation: Vec::new(),
        hosted_tools: Vec::new(),
        model: "grok/test-model".parse().expect("model"),
        model_params: ModelParams::default(),
        tool_choice: ToolChoice::Auto,
        originator: PromptOriginator::User,
        ctx_id: None,
        compaction: None,
        operation: PromptOperation::Inference,
    }
}

/// Selected model identity; tests do not depend on a current production slug.
fn model() -> AttemptModel {
    AttemptModel {
        id: ModelName::new("test-model"),
    }
}

/// Read only owned semantic fields in assertions.
fn json_request(prompt: &AgentPromptCreated, native_images: bool) -> Value {
    let request = Request::lower(prompt, &model(), 123, &[], native_images).expect("request");
    serde_json::from_str(request.prepared().json().get()).expect("JSON")
}

/// Storage, reasoning replay and sticky routing are xAI policy, not OpenAI TTL.
#[test]
fn request_uses_xai_policy_and_stable_routing() {
    let prompt = prompt();
    let first = json_request(&prompt, false);
    assert_eq!(first["store"], false);
    assert_eq!(first["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(first["prompt_cache_key"], "tau:agent-test");
    assert_eq!(first["max_output_tokens"], 123);
    for absent in ["prompt_cache_options", "previous_response_id", "reasoning"] {
        assert!(first.get(absent).is_none(), "{absent}");
    }
    let mut next = prompt.clone();
    next.agent_prompt_id = "next-prompt".parse().expect("prompt id");
    next.session_id = "next-session".parse().expect("session id");
    assert_eq!(json_request(&next, false), first);
    next.agent_id = AgentId::parse("other-agent").expect("agent id");
    assert_ne!(
        json_request(&next, false)["prompt_cache_key"],
        first["prompt_cache_key"]
    );
}

/// Never submit an unadvertised selector or replace an alias-specific default.
#[test]
fn request_checks_exact_advertised_reasoning_effort() {
    let mut prompt = prompt();
    prompt.model_params.effort = ReasoningSelection::native(NativeReasoningEffort::High);
    assert!(Request::lower(&prompt, &model(), 0, &[], false).is_err());
    assert!(Request::lower(&prompt, &model(), 0, &["High".into()], false).is_err());
    let request =
        Request::lower(&prompt, &model(), 0, &["high".into()], false).expect("advertised selector");
    let value: Value = serde_json::from_str(request.prepared().json().get()).expect("JSON");
    assert_eq!(value["reasoning"], json!({"effort":"high"}));
    assert!(value.get("max_output_tokens").is_none());
}

/// Construct one canonical successful tool result with typed image bytes.
pub(super) fn image_result() -> ToolResultItem {
    ToolResultItem {
        call_id: ToolCallId::new("image-call"),
        tool_type: ToolType::Function,
        status: ToolResultStatus::Success,
        output: ToolResponse::from_cbor(&CborValue::Text("tool text".into())),
        presentation: Default::default(),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: b"abc".to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
    }
}

/// Typed images stay in their tool result while exact opaque JSON survives.
#[test]
fn request_preserves_opaque_replay_and_native_tool_images() {
    let raw_reasoning = r#"{"type":"reasoning","id":"rs_test","encrypted_content":"sealed","summary":[],"number":1.2300,"extra\u005fkey":"preserved"}"#;
    let mut prompt = prompt();
    prompt
        .context
        .blocks
        .push(ContextBlock::AssistantResponse(AssistantResponseBlock {
            provider_response_id: Some("old-response".into()),
            backend: None,
            usage: None,
            output_items: vec![ContextItem::Reasoning(
                OpaqueProviderItem::from_raw_json(raw_reasoning).expect("reasoning"),
            )],
        }));
    prompt
        .context
        .blocks
        .push(ContextBlock::ToolResults(ToolResultsBlock {
            items: vec![image_result()],
        }));
    let request = Request::lower(&prompt, &model(), 0, &[], true).expect("request");
    assert!(request.prepared().json().get().contains(raw_reasoning));
    let value: Value = serde_json::from_str(request.prepared().json().get()).expect("JSON");
    let input = value["input"].as_array().expect("input");
    assert_eq!(input.len(), 2);
    assert_eq!(
        input[1],
        json!({
            "type":"function_call_output",
            "call_id":"image-call",
            "output":[
                {"type":"input_text","text":image_result().render_provider_text()},
                {"type":"input_image","image_url":"data:image/png;base64,YWJj","detail":"high"},
            ]
        })
    );
    let unsupported = json_request(&prompt, false);
    assert!(
        unsupported["input"][1]["output"]
            .as_str()
            .expect("text")
            .contains("image omitted")
    );
    assert!(!unsupported.to_string().contains("base64"));
}

/// JSON-escaped canonical call IDs still map to their ordered tool results.
#[test]
fn request_maps_escaped_call_ids_without_rewriting_identity() {
    let mut prompt = prompt();
    let mut result = image_result();
    let call_id = "quoted\"and\\escaped";
    result.call_id = ToolCallId::new(call_id);
    prompt
        .context
        .blocks
        .push(ContextBlock::ToolResults(ToolResultsBlock {
            items: vec![result],
        }));
    let request = json_request(&prompt, true);
    assert_eq!(request["input"][0]["call_id"], call_id);
    assert_eq!(request["input"][0]["output"][1]["detail"], "high");
}

/// Unsupported operations fail locally, rather than silently sampling normally.
#[test]
fn request_rejects_standalone_compaction_and_unsupported_tools() {
    let mut prompt = prompt();
    prompt.operation = PromptOperation::StandaloneCompaction;
    assert!(Request::lower(&prompt, &model(), 0, &[], false).is_err());
    prompt.operation = PromptOperation::Inference;
    let mut result = image_result();
    result.tool_type = ToolType::Custom;
    prompt
        .context
        .blocks
        .push(ContextBlock::ToolResults(ToolResultsBlock {
            items: vec![result],
        }));
    assert!(Request::lower(&prompt, &model(), 0, &[], true).is_err());
}
