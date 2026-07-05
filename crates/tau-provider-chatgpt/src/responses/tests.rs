use tau_proto::{
    ContentPart, ContextItem, ContextRole, MessageItem, OpaqueProviderItem, ToolCallItem,
    ToolResultItem, ToolResultStatus,
};

use super::*;
use crate::common::LlmError;

fn unique_temp_state_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tau-provider-chatgpt-state-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn context(items: &[ContextItem]) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: items.to_vec(),
            },
        )],
    }))
}

fn context_with_response_id(
    response_id: &str,
    before: Vec<ContextItem>,
    assistant: Vec<ContextItem>,
    after: Vec<ContextItem>,
) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: before }),
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: Some(response_id.to_owned()),
                backend: None,
                output_items: assistant,
                usage: None,
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: after }),
        ],
    }))
}

#[test]
fn debug_provider_request_dir_requires_existing_session_dir() {
    // Provider diagnostics are allowed to create their own debug subdirectory,
    // but must not create durable per-session roots for ephemeral sessions.
    let state_dir = unique_temp_state_dir("missing-session");
    let session_id = "session-missing";

    assert!(debug_provider_request_dir_in(&state_dir, session_id, true).is_none());
    assert!(
        !state_dir.join("sessions").join(session_id).exists(),
        "missing session dir should not be created"
    );
}

#[test]
fn debug_provider_request_dir_returns_debug_dir_for_existing_session() {
    // Durable sessions create their session directory before provider calls; in
    // that case provider diagnostics can write under the standard debug path.
    let state_dir = unique_temp_state_dir("existing-session");
    let session_id = "session-existing";
    let session_dir = state_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).expect("create durable session dir");

    assert_eq!(
        debug_provider_request_dir_in(&state_dir, session_id, true),
        Some(session_dir.join("debug").join("provider-requests"))
    );
}

#[test]
fn debug_provider_request_dir_rejects_ephemeral_session_with_existing_dir() {
    // Explicit session persistence state wins over filesystem shape: an
    // ephemeral current session can reuse an id that has an old durable
    // directory, and provider diagnostics must still stay disabled.
    let state_dir = unique_temp_state_dir("ephemeral-reuse");
    let session_id = "session-reused";
    let session_dir = state_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).expect("create old durable session dir");

    assert!(debug_provider_request_dir_in(&state_dir, session_id, false).is_none());
}

#[test]
fn build_request_includes_prompt_cache_key_when_supported() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        context_window: 258400,
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: true,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let prompt_cache_key = body["prompt_cache_key"].as_str().expect("prompt_cache_key");

    assert!(uuid::Uuid::parse_str(prompt_cache_key).is_ok());
}

#[test]
fn build_request_includes_service_tier_when_configured() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        context_window: 258400,
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["service_tier"], "priority");
}

/// Tau's `off` is a user-visible request for no reasoning, not "use the
/// provider default". GPT-5.5 defaults omitted effort to `medium`, so the
/// OpenAI provider must send the explicit `none` value when effort is off.
#[test]
fn build_request_maps_off_effort_to_openai_none() {
    let config = ResponsesConfig {
        supports_reasoning_effort: true,
        ..chain_test_config()
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["reasoning"]["effort"], "none");
}

#[test]
fn build_request_omits_prompt_cache_key_without_seed() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        context_window: 258400,
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("prompt_cache_key"));
}

/// First turn (no chain established): the request must contain the
/// full transcript, `store: false`, and no `previous_response_id`.
/// This is the baseline that future stateful-chain optimizations are
/// compared against; if it ever flips, every turn would start
/// charging for stored responses by accident.
#[test]
fn build_request_first_turn_replays_full_history_without_chain() {
    let config = chain_test_config();
    let messages = vec![user_text("hello"), assistant_text("hi there")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    // Two messages → two `input` items (one user text, one assistant message).
    assert_eq!(
        input.len(),
        2,
        "full history must be replayed when chain is absent"
    );
}

/// Regression: a daemon restore can repair an interrupted foreground tool call
/// by writing a durable synthetic internal tool error before the user sends the
/// next prompt. With no chain anchor after restore, Responses must replay the
/// repaired assistant tool call, its matching output, and the new user message
/// without sending a stale `previous_response_id`.
#[test]
fn build_request_full_replay_serializes_restored_tool_error_before_next_user_message() {
    let config = chain_test_config();
    let messages = vec![
        assistant_tool_call(
            "call-restored",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Map(vec![(
                tau_proto::CborValue::Text("command".to_owned()),
                tau_proto::CborValue::Text("sleep 30".to_owned()),
            )]),
        ),
        restored_internal_tool_error("call-restored", "partial stdout before restart"),
        user_text("after restart"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request body is an object");
    assert!(
        object.get("previous_response_id").is_none(),
        "restored full replay must not send a stale chain id"
    );

    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        3,
        "restored full replay must keep the repaired tool round balanced"
    );
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["id"], "fc_call-restored");
    assert_eq!(input[0]["call_id"], "call-restored");
    assert_eq!(input[0]["arguments"], "{\"command\":\"sleep 30\"}");
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "call-restored");
    let output = input[1]["output"].as_str().expect("tool output");
    assert!(output.contains("error: tau_internal: true"));
    assert!(output.contains("Tool call `call-restored` was interrupted"));
    assert!(output.contains("partial stdout before restart"));
    assert_eq!(input[2]["role"], "user");
    assert_eq!(input[2]["content"][0]["text"], "after restart");
}

/// Responses replay must use the provider's original function-call argument
/// string when it is available. Re-serializing parsed CBOR can reorder keys or
/// normalize whitespace/numbers, which changes the provider-visible cache
/// input.
#[test]
fn build_request_full_replay_preserves_raw_function_call_arguments() {
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    let messages = vec![
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call-raw".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Map(vec![
                (
                    tau_proto::CborValue::Text("z".to_owned()),
                    tau_proto::CborValue::Integer(1.into()),
                ),
                (
                    tau_proto::CborValue::Text("a".to_owned()),
                    tau_proto::CborValue::Array(vec![
                        tau_proto::CborValue::Integer(2.into()),
                        tau_proto::CborValue::Integer(3.into()),
                    ]),
                ),
            ]),
            raw_arguments_json: Some(raw_arguments.to_owned()),
        }),
        user_text("continue"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&chain_test_config(), &request, None))
        .expect("serialize request");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["arguments"], raw_arguments);
}

/// Stateful-chain turn: when the harness supplies a
/// `previous_response`, the request body slices off the prefix
/// already covered by that response and pins the prior `response.id`.
/// `store` stays `false` — the Codex endpoint *rejects* `store: true`
/// (`HTTP 400 {"detail":"Store must be set to false"}`) even when
/// chaining, in contrast with the public Responses API. Tau today
/// only routes Responses through Codex, so this asserts the Codex
/// shape; a future public-API path would need a separate test.
#[test]
fn build_request_chain_turn_sends_delta_and_previous_response_id() {
    let config = chain_test_config();
    // Full transcript: 1 user, 1 assistant response, 1 user.
    // The cached response id was captured after the assistant turn, so only
    // the trailing user message should make it into the request.
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_abc",
            vec![user_text("first turn")],
            vec![assistant_text("first response")],
            vec![user_text("second turn")],
        ),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, Some("resp_abc")))
        .expect("serialize");

    assert_eq!(
        body["store"], false,
        "Codex rejects store=true even when chaining"
    );
    assert_eq!(body["previous_response_id"], "resp_abc");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        1,
        "only messages after the anchor should be sent"
    );
    assert_eq!(input[0]["content"][0]["text"], "second turn");
}

/// Defensive: a cached response id missing from the prompt context must NOT
/// chain — fall back to a full-replay first-turn-style request so the
/// conversation keeps working instead of sending an invalid delta.
#[test]
fn build_request_cached_response_missing_from_context_falls_back_to_full_replay() {
    let config = chain_test_config();
    let messages = vec![user_text("only")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body =
        serde_json::to_value(build_request(&config, &request, Some("missing"))).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
}

/// Regression: `prompt_cache_key` must still ride along on chained
/// (`previous_response_id`) turns. Without it the Codex backend would
/// route the chain continuation to a different machine on each turn
/// and squander the warm cache the chain is supposed to preserve.
#[test]
fn build_request_chain_turn_still_emits_prompt_cache_key() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_abc",
            vec![user_text("first turn")],
            vec![assistant_text("first response")],
            vec![user_text("second turn")],
        ),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, Some("resp_abc")))
        .expect("serialize");
    assert_eq!(body["previous_response_id"], "resp_abc");
    assert!(body["prompt_cache_key"].is_string());
}

/// The Responses backend must keep the wire `prompt_cache_key` stable for the
/// same target agent even when prompt provenance changes. This pins the
/// regression where manager-relayed sub-agent messages changed originator and
/// cold-started the provider cache bucket.
#[test]
fn build_request_prompt_cache_key_ignores_originator() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("__harness__"),
        query_id: "delegate-1".into(),
    };
    let user_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &ext,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let user_body =
        serde_json::to_value(build_request(&config, &user_request, None)).expect("serialize");
    let ext_body =
        serde_json::to_value(build_request(&config, &ext_request, None)).expect("serialize");

    assert!(user_body["prompt_cache_key"].is_string());
    assert!(ext_body["prompt_cache_key"].is_string());
    assert_eq!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
}

/// The legacy `share_user_cache_key` flag should no longer be needed to make an
/// extension-originated prompt use the stable per-agent cache key. Keeping this
/// no-op avoids letting the flag reintroduce a provenance-derived bucket split.
#[test]
fn build_request_share_user_cache_key_does_not_change_agent_bucket() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let shared_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &ext,
        share_user_cache_key: true,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    let body =
        serde_json::to_value(build_request(&config, &shared_request, None)).expect("serialize");
    let default_request = PromptPayload {
        share_user_cache_key: false,
        debug_provider_requests: false,
        ..shared_request
    };
    let default_body =
        serde_json::to_value(build_request(&config, &default_request, None)).expect("serialize");
    assert!(body["prompt_cache_key"].is_string());
    assert_eq!(body["prompt_cache_key"], default_body["prompt_cache_key"]);
}

#[test]
fn build_request_extension_matches_user_wire_body_for_same_context() {
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run shell commands".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let user_context = context_with_response_id(
        "resp_parent",
        vec![user_text("parent prompt")],
        vec![assistant_text("parent response")],
        vec![user_text("summarize")],
    );
    let ext_context = context_with_response_id(
        "resp_parent",
        vec![user_text("parent prompt")],
        vec![assistant_text("parent response")],
        vec![user_text("summarize")],
    );
    let user_request = PromptPayload {
        system_prompt: "sys",
        context: user_context,
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        context: ext_context,
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &ext,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let user_body =
        serde_json::to_value(build_request(&config, &user_request, Some("resp_parent")))
            .expect("serialize");
    let ext_body = serde_json::to_value(build_request(&config, &ext_request, Some("resp_parent")))
        .expect("serialize");

    assert_eq!(ext_body, user_body);
    assert_eq!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
    assert_eq!(ext_body["tool_choice"], "auto");
    assert_eq!(ext_body["previous_response_id"], "resp_parent");
}

/// `ToolChoice::None` emits `tool_choice: "none"` on the Responses
/// body while leaving the `tools` array fully declared. That is valid
/// for callers that intentionally want a different wire request, but
/// the harness must not use it for cache-sharing side queries because
/// the field participates in provider request equivalence. Verified
/// here on a request that carries real tool definitions.
#[test]
fn build_request_emits_tool_choice_none_while_keeping_tools_declared() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::None,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["tool_choice"], "none");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "tools must stay declared so the cache prefix matches"
    );
}

#[test]
fn build_request_sends_compaction_context_management_and_trigger_item() {
    let config = ResponsesConfig {
        supports_compaction: true,
        ..chain_test_config()
    };
    let items = [ContextItem::CompactionTrigger];
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&items),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: Some(1200),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["context_management"][0]["type"], "compaction");
    assert_eq!(body["context_management"][0]["compact_threshold"], 1200);
    assert_eq!(body["input"][0]["type"], "compaction_trigger");
}

#[test]
fn build_request_trims_full_replay_before_latest_compaction_item() {
    let config = ResponsesConfig {
        supports_compaction: true,
        ..chain_test_config()
    };
    let compaction_item = serde_json::json!({
        "type": "compaction",
        "summary": "old history",
    });
    let items = [
        user_text("obsolete"),
        ContextItem::Compaction(OpaqueProviderItem(crate::common::json_to_cbor(
            &compaction_item,
        ))),
        user_text("new"),
    ];
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&items),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: None,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input array");

    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "compaction");
    assert_eq!(input[1]["content"][0]["text"], "new");
    assert_eq!(body["context_management"][0]["compact_threshold"], 232560);
}

fn chain_test_config() -> ResponsesConfig {
    ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        context_window: 258400,
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_websocket: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    }
}

fn phase_test_config() -> ResponsesConfig {
    ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_phase: true,
        ..chain_test_config()
    }
}

fn encrypted_reasoning_test_config() -> ResponsesConfig {
    ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        supports_encrypted_reasoning: true,
        ..chain_test_config()
    }
}

fn user_text(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
    })
}

fn assistant_text(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
    })
}

fn assistant_text_with_phase(text: &str, phase: tau_proto::MessagePhase) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: Some(phase),
    })
}

fn assistant_tool_call(
    id: &str,
    name: &str,
    tool_type: tau_proto::ToolType,
    input: tau_proto::CborValue,
) -> ContextItem {
    ContextItem::ToolCall(ToolCallItem {
        call_id: id.into(),
        name: tau_proto::ToolName::new(name),
        tool_type,
        arguments: input,
        raw_arguments_json: None,
    })
}

fn restored_internal_tool_error(call_id: &str, body: &str) -> ContextItem {
    ContextItem::ToolResult(ToolResultItem {
        call_id: call_id.into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Error {
            message: format!(
                "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
                tau_proto::TAU_INTERNAL_HEADER_NAME
            ),
        },
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(body.to_owned())),
    })
}

fn reasoning_item(item: &str) -> ContextItem {
    let value: serde_json::Value = serde_json::from_str(item).expect("reasoning item json");
    ContextItem::Reasoning(OpaqueProviderItem(crate::common::json_to_cbor(&value)))
}

/// When `supports_phase` is on, every assistant `message` item must
/// carry a `phase` field. A stored `Commentary` value rides straight
/// through; absence of a stored value falls back to `final_answer`
/// per the OpenAI deployment-checklist guidance for legacy history.
#[test]
fn build_request_stamps_phase_on_assistant_messages_when_supported() {
    let config = phase_test_config();
    let messages = vec![
        user_text("hello"),
        assistant_text_with_phase("draft", tau_proto::MessagePhase::Commentary),
        user_text("more"),
        assistant_text("legacy turn without phase"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");

    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(assistant_items.len(), 2, "two assistant messages expected");
    assert_eq!(assistant_items[0]["phase"], "commentary");
    assert_eq!(
        assistant_items[1]["phase"], "final_answer",
        "legacy assistant message must default to final_answer per OpenAI guidance"
    );
}

/// `supports_phase: false` keeps the field off the wire entirely,
/// even when the stored message carries one. This is the safety
/// gate that lets older Codex models (which would reject unknown
/// fields) keep working as the harness sends them history that may
/// have been captured against a newer model.
#[test]
fn build_request_omits_phase_when_unsupported() {
    let config = chain_test_config(); // supports_phase: false
    let messages = vec![assistant_text_with_phase(
        "draft",
        tau_proto::MessagePhase::Commentary,
    )];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_item = input
        .iter()
        .find(|i| i["role"].as_str() == Some("assistant"))
        .expect("assistant message");
    assert!(
        assistant_item
            .as_object()
            .expect("assistant item is an object")
            .get("phase")
            .is_none(),
        "phase must not be sent when the backend doesn't advertise support"
    );
}

/// Tool-call boundaries flush a pending text block into its own
/// assistant `message` item. That intermediate flush must carry
/// `phase` too — otherwise a mixed text+tool_use turn would
/// half-pass-through with a phase on the trailing flush only.
#[test]
fn build_request_stamps_phase_on_pre_tool_call_text_flush() {
    let config = phase_test_config();
    let messages = vec![
        assistant_text_with_phase("thinking out loud", tau_proto::MessagePhase::Commentary),
        assistant_tool_call(
            "call-1",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Null,
        ),
        assistant_text_with_phase("trailing", tau_proto::MessagePhase::Commentary),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(
        assistant_items.len(),
        2,
        "pre-tool-call text and trailing text each become their own assistant message"
    );
    for (i, item) in assistant_items.iter().enumerate() {
        assert_eq!(
            item["phase"], "commentary",
            "assistant message #{i} must carry the captured phase"
        );
    }
}

/// `parse_phase_from_item` is the bridge from the Responses-API
/// `output_item.*` shape into our typed `MessagePhase`. The two
/// known wire strings round-trip; unknown values (forward
/// compatibility) come back as `None` rather than panicking.
#[test]
fn parse_phase_from_item_recognizes_wire_strings() {
    let commentary = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&commentary),
        Some(tau_proto::MessagePhase::Commentary)
    );

    let final_ans = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "final_answer",
    });
    assert_eq!(
        parse_phase_from_item(&final_ans),
        Some(tau_proto::MessagePhase::FinalAnswer)
    );

    let unknown_future = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "rumination",
    });
    assert_eq!(parse_phase_from_item(&unknown_future), None);

    let no_phase = serde_json::json!({
        "type": "message",
        "role": "assistant",
    });
    assert_eq!(parse_phase_from_item(&no_phase), None);

    let function_call = serde_json::json!({
        "type": "function_call",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&function_call),
        None,
        "non-message items must not have their `phase` field harvested"
    );
}

// -----------------------------------------------------------------------
// Encrypted reasoning replay
// -----------------------------------------------------------------------

/// `supports_encrypted_reasoning: true` must put
/// `include: ["reasoning.encrypted_content"]` on the request body.
/// Without this opt-in the model returns `reasoning` items but with
/// no replayable content — we'd persist empty husks and lose the
/// continuity the whole feature buys.
#[test]
fn build_request_emits_include_when_encrypted_reasoning_supported() {
    let config = encrypted_reasoning_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let include = body["include"].as_array().expect("include array");
    assert_eq!(include.len(), 1);
    assert_eq!(include[0], "reasoning.encrypted_content");
}

/// `supports_encrypted_reasoning: false` keeps the `include` field
/// out of the request entirely — older endpoints (and the public
/// Responses API) reject unknown opt-ins, so we don't even want an
/// empty `include: []` on the wire.
#[test]
fn build_request_omits_include_when_encrypted_reasoning_unsupported() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("include")
            .is_none(),
        "include must be omitted when the provider doesn't advertise support"
    );
}

/// A `ContentBlock::Reasoning` on an assistant message must be
/// emitted as its own top-level `input[]` item — same structural
/// slot as `message` and `function_call`, NEVER nested inside the
/// assistant message. Locks in the Pi-compatible replay shape; if
/// this regresses, the model loses reasoning continuity across a
/// broken chain.
#[test]
fn build_request_replays_reasoning_item_as_top_level_input() {
    let config = encrypted_reasoning_test_config();
    let reasoning_blob = serde_json::json!({
        "type": "reasoning",
        "id": "rs_abc123",
        "summary": [],
        "encrypted_content": "OPAQUE-BLOB"
    })
    .to_string();
    let messages = vec![
        reasoning_item(&reasoning_blob),
        assistant_text("here's the answer"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let reasoning_idx = input
        .iter()
        .position(|item| item["type"].as_str() == Some("reasoning"))
        .expect("reasoning input item");
    let message_idx = input
        .iter()
        .position(|item| item["role"].as_str() == Some("assistant"))
        .expect("assistant message item");
    assert!(
        reasoning_idx < message_idx,
        "reasoning must precede the assistant message it relates to (Pi-compatible order); \
         reasoning_idx={reasoning_idx}, message_idx={message_idx}"
    );
    let reasoning = &input[reasoning_idx];
    assert_eq!(reasoning["id"], "rs_abc123");
    assert_eq!(
        reasoning["encrypted_content"], "OPAQUE-BLOB",
        "the opaque blob must round-trip verbatim — the harness must not parse fields out"
    );
}

#[test]
fn build_request_emits_custom_tool_definition_and_round_trips_custom_tool_output() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("apply_patch"),
        model_visible_name: None,
        description: Some("Apply a patch to files".to_owned()),
        tool_type: tau_proto::ToolType::Custom,
        parameters: None,
        format: Some(tau_proto::ToolFormat::Grammar {
            syntax: tau_proto::ToolGrammarSyntax::Regex,
            definition: "(?s).+".to_owned(),
        }),
    };
    let messages = vec![
        assistant_tool_call(
            "call-patch",
            "apply_patch",
            tau_proto::ToolType::Custom,
            tau_proto::CborValue::Text("*** Begin Patch\n*** End Patch".into()),
        ),
        ContextItem::ToolResult(ToolResultItem {
            call_id: "call-patch".into(),
            tool_type: tau_proto::ToolType::Custom,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("ok".into())),
        }),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["type"], "custom");
    assert_eq!(tools[0]["name"], "apply_patch");
    assert_eq!(tools[0]["format"]["type"], "grammar");
    assert_eq!(tools[0]["format"]["syntax"], "regex");
    assert_eq!(tools[0]["format"]["definition"], "(?s).+");

    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "custom_tool_call");
    assert_eq!(input[0]["id"], "ctc_call-patch");
    assert_eq!(input[0]["call_id"], "call-patch");
    assert_eq!(input[0]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(input[1]["type"], "custom_tool_call_output");
    assert_eq!(input[1]["call_id"], "call-patch");
    assert_eq!(input[1]["output"], "ok");
}

/// Provider item ids are separate from Tau call ids and must be prefixed for
/// the Responses API, but replay can contain ids already captured from that
/// API. Pin both cases so request conversion does not double-prefix history
/// or accidentally send unprefixed local ids.
#[test]
fn build_request_preserves_existing_provider_tool_call_id_prefixes() {
    let config = chain_test_config();
    let messages = vec![
        assistant_tool_call(
            "fc_existing",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Null,
        ),
        assistant_tool_call(
            "ctc_existing",
            "apply_patch",
            tau_proto::ToolType::Custom,
            tau_proto::CborValue::Text("patch".into()),
        ),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["id"], "fc_existing");
    assert_eq!(input[0]["call_id"], "fc_existing");
    assert_ne!(input[0]["id"], "fc_fc_existing");
    assert_eq!(input[1]["type"], "custom_tool_call");
    assert_eq!(input[1]["id"], "ctc_existing");
    assert_eq!(input[1]["call_id"], "ctc_existing");
    assert_ne!(input[1]["id"], "ctc_ctc_existing");
}

/// Cancelled tool results are replayed as provider output items rather than
/// dropped. The rendered cancellation header is the only durable explanation
/// the model sees on replay, so keep its wire type, call id, and text shape
/// pinned across conversion refactors.
#[test]
fn build_request_replays_cancelled_tool_result_with_header() {
    let config = chain_test_config();
    let messages = vec![ContextItem::ToolResult(ToolResultItem {
        call_id: "call-cancelled".into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Cancelled {
            reason: "user interrupted".to_owned(),
        },
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
    })];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "function_call_output");
    assert_eq!(input[0]["call_id"], "call-cancelled");
    assert_eq!(input[0]["output"], "cancelled: user interrupted\n\n");
}

#[test]
fn apply_event_accumulates_custom_tool_input_deltas() {
    use crate::common::StreamState;

    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
        }
    });
    apply_event(&mut state, &added, &mut |_| {}).expect("added");
    let delta = serde_json::json!({
        "type": "response.custom_tool_call_input.delta",
        "output_index": 0,
        "delta": "*** Begin Patch"
    });
    apply_event(&mut state, &delta, &mut |_| {}).expect("delta");
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
            "input": "*** Begin Patch"
        }
    });
    apply_event(&mut state, &done, &mut |_| {}).expect("done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected custom tool call item");
    };
    assert_eq!(call.tool_type, tau_proto::ToolType::Custom);
    assert_eq!(call.call_id.as_str(), "call_patch");
    assert_eq!(call.name.as_str(), "apply_patch");
    assert_eq!(call.raw_arguments_json, None);
    assert_eq!(
        call.arguments,
        tau_proto::CborValue::Text("*** Begin Patch".into())
    );
}

#[test]
fn build_request_chain_keeps_custom_tool_output_type_from_prior_history() {
    let config = chain_test_config();
    let tool_result = ToolResultItem {
        call_id: "call-custom".into(),
        tool_type: tau_proto::ToolType::Custom,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("ok".into())),
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: Box::leak(Box::new(tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: Some("resp_custom".to_owned()),
                    backend: None,
                    output_items: vec![assistant_tool_call(
                        "call-custom",
                        "apply_patch",
                        tau_proto::ToolType::Custom,
                        tau_proto::CborValue::Text("patch body".into()),
                    )],
                    usage: None,
                }),
                tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                    items: vec![tool_result],
                }),
            ],
        })),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, Some("resp_custom")))
        .expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(
        input.len(),
        1,
        "only the trailing tool result should be replayed"
    );
    assert_eq!(input[0]["type"], "custom_tool_call_output");
    assert_eq!(input[0]["call_id"], "call-custom");
}

/// On the Codex Responses stream, `response.output_item.done` is the
/// canonical place to capture a `reasoning` item: it's the only
/// event that carries the final `encrypted_content`. The `added`
/// counterpart fires before any content streams in, so capturing
/// from `added` would persist empty husks. Pin the boundary here so
/// a future refactor of the SSE/WS parser can't silently swap which
/// event we read.
#[test]
fn apply_event_captures_reasoning_only_on_output_item_done() {
    use crate::common::StreamState;
    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_pending",
            "summary": [],
        }
    });
    apply_event(&mut state, &added, &mut |_| {}).expect("added");
    assert!(
        state
            .output_items
            .iter()
            .all(|item| matches!(item, crate::common::OutputItemAccumulator::Empty)),
        "`added` carries no encrypted_content — capturing here would persist an empty husk"
    );
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_done",
            "summary": [{"type": "summary_text", "text": "thought"}],
            "encrypted_content": "SEALED",
        }
    });
    apply_event(&mut state, &done, &mut |_| {}).expect("done");
    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::Reasoning(item) = &items[0] else {
        panic!("expected reasoning item");
    };
    let parsed = crate::common::cbor_to_json(&item.0);
    assert_eq!(parsed["id"], "rs_done");
    assert_eq!(parsed["encrypted_content"], "SEALED");
}

/// Server-side compaction is returned as an ordinary Responses output item.
/// Keep it in the same ordered item accumulator as messages, reasoning, and
/// tool calls so a compaction item is durable transcript output rather than a
/// side channel that can be lost.
#[test]
fn apply_event_captures_compaction_output_item_in_order() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "before",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("message done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "compaction",
                "summary": "old history",
                "input_items": [{
                    "type": "message",
                    "role": "user",
                    "content": "compacted",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("compaction done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    assert!(matches!(items[0], tau_proto::ContextItem::Message(_)));
    let tau_proto::ContextItem::Compaction(item) = &items[1] else {
        panic!("expected compaction item");
    };
    let parsed = crate::common::cbor_to_json(&item.0);
    assert_eq!(parsed["type"], "compaction");
    assert_eq!(parsed["summary"], "old history");
}

// -----------------------------------------------------------------------
// WebSocket envelope wrapping
// -----------------------------------------------------------------------

/// The WS guide requires every client frame to carry `type:
/// "response.create"` at the top level. The HTTP body does not.
/// [`build_ws_envelope`] is the only place we add the tag — pin it
/// here so a future refactor that drops the wrapper struct can't
/// silently regress it.
#[test]
fn ws_envelope_adds_type_and_drops_stream() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let http_body =
        serde_json::to_value(build_request(&config, &request, None)).expect("http body");
    let ws_body = serde_json::to_value(build_ws_envelope(&config, &request, None, None))
        .expect("ws envelope");

    assert_eq!(ws_body["type"], "response.create");
    let ws_object = ws_body.as_object().expect("WS envelope object");
    assert!(
        ws_object.get("stream").is_none(),
        "WS frame must omit `stream` — the WS guide says it's not used and the field is transport-implicit"
    );
    // Every other body shape stays identical so the request-build
    // tests already in this file double as WS-envelope coverage.
    assert!(
        ws_object.get("generate").is_none(),
        "normal streaming WS turns must preserve the old wire shape"
    );
    assert_eq!(ws_body["model"], http_body["model"]);
    assert_eq!(ws_body["store"], http_body["store"]);
    assert_eq!(ws_body["input"], http_body["input"]);
}

#[test]
fn ws_prewarm_envelope_sets_generate_false_and_drops_previous_response() {
    let config = chain_test_config();
    let messages = vec![user_text("AGENTS.md context")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::new("test-session"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_ws_envelope(&config, &request, None, Some(false)))
        .expect("prewarm envelope");

    assert_eq!(body["type"], "response.create");
    assert_eq!(body["generate"], false);
    let object = body.as_object().expect("prewarm envelope object");
    assert!(object.get("stream").is_none());
    assert!(object.get("previous_response_id").is_none());
}

// -----------------------------------------------------------------------
// apply_event — shared event applicator for SSE + WS
// -----------------------------------------------------------------------

/// `response.output_text.delta` accumulates into `state.text` and
/// fires `on_update` once per delta. Mirrors the original SSE-only
/// behavior — keeps the WS path equivalent.
#[test]
fn apply_event_text_delta_accumulates_and_notifies() {
    let mut state = crate::common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |state: &crate::common::StreamState| {
        updates.push(state.text.clone());
    };

    for chunk in ["hel", "lo, ", "world"] {
        let ev = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": chunk,
        });
        let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
        assert!(!done, "text delta should not terminate the stream");
    }
    assert_eq!(state.text, "hello, world");
    assert_eq!(updates, vec!["hel", "hello, ", "hello, world"]);
}

#[test]
fn stream_delta_emitter_emits_only_new_assistant_and_reasoning_text() {
    // Streaming response updates are append deltas; this prevents large
    // responses from being copied and sent again on every provider chunk while
    // keeping the final output item accumulator complete.
    let mut state = crate::common::StreamState::new();
    let mut emitter = crate::common::StreamDeltaEmitter::default();

    state.append_message_delta_at(0, "hel");
    state.append_reasoning_summary_delta_at(1, "plan");
    assert_eq!(
        emitter.deltas(&state),
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "hel".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Summary,
                text: "plan".to_owned(),
            },
        ]
    );

    state.append_message_delta_at(0, "lo");
    state.append_reasoning_summary_delta_at(1, " next");
    assert_eq!(
        emitter.deltas(&state),
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "lo".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Summary,
                text: " next".to_owned(),
            },
        ]
    );
    assert!(emitter.deltas(&state).is_empty());
}

#[test]
fn apply_event_preserves_incremental_output_item_order() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "rs_ordered",
                "encrypted_content": "OPAQUE",
            },
        }),
        &mut on_update,
    )
    .expect("reasoning done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
            },
        }),
        &mut on_update,
    )
    .expect("message added");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 1,
            "delta": "checking",
        }),
        &mut on_update,
    )
    .expect("text delta");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 2,
            "item": {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
            },
        }),
        &mut on_update,
    )
    .expect("tool added");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.done",
            "output_index": 2,
            "arguments": "{\"path\":\"Cargo.toml\"}",
        }),
        &mut on_update,
    )
    .expect("tool args done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 3);
    assert!(matches!(items[0], tau_proto::ContextItem::Reasoning(_)));
    let tau_proto::ContextItem::Message(message) = &items[1] else {
        panic!("expected message item");
    };
    assert_eq!(message.phase, Some(tau_proto::MessagePhase::Commentary));
    assert!(matches!(
        &message.content[0],
        tau_proto::ContentPart::Text { text } if text == "checking"
    ));
    let tau_proto::ContextItem::ToolCall(call) = &items[2] else {
        panic!("expected tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_read");
    assert_eq!(call.name.as_str(), "read");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"path\":\"Cargo.toml\"}")
    );
    assert_eq!(
        crate::common::cbor_to_json(&call.arguments),
        serde_json::json!({ "path": "Cargo.toml" })
    );
}

/// Some Responses streams with tool calls commit assistant message
/// text only on `response.output_item.done`, without earlier
/// `response.output_text.delta` events. Persist that message in item
/// order so commentary immediately before a tool call is not dropped.
#[test]
fn apply_event_output_item_done_hydrates_message_text_before_tool_call() {
    let mut state = crate::common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |state: &crate::common::StreamState| {
        if updates.last() != Some(&state.text) {
            updates.push(state.text.clone());
        }
    };

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{
                    "type": "output_text",
                    "text": "I'll inspect the file first.",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("message done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
                "arguments": "{\"path\":\"Cargo.toml\"}",
            },
        }),
        &mut on_update,
    )
    .expect("tool done");

    assert_eq!(updates, vec!["I'll inspect the file first."]);
    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    let tau_proto::ContextItem::Message(message) = &items[0] else {
        panic!("expected message item before tool call");
    };
    assert_eq!(message.phase, Some(tau_proto::MessagePhase::Commentary));
    assert!(matches!(
        &message.content[0],
        tau_proto::ContentPart::Text { text } if text == "I'll inspect the file first."
    ));
    let tau_proto::ContextItem::ToolCall(call) = &items[1] else {
        panic!("expected tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_read");
    assert_eq!(call.name.as_str(), "read");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"path\":\"Cargo.toml\"}")
    );
}

#[test]
fn apply_event_completed_does_not_harvest_response_output() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    let done = apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_final",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{
                        "type": "output_text",
                        "text": "must not be harvested",
                    }],
                }],
            },
        }),
        &mut on_update,
    )
    .expect("completed");

    assert!(done);
    assert_eq!(state.response_id.as_deref(), Some("resp_final"));
    assert_eq!(state.text, "");
    assert!(state.into_output_items().is_empty());
}

#[test]
fn apply_event_completed_terminates_and_captures_response_id() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_xyz",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 5 },
            },
        },
    });
    let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
    assert!(done, "response.completed must terminate the stream");
    assert_eq!(state.response_id.as_deref(), Some("resp_xyz"));
    assert_eq!(state.input_tokens, Some(42));
    assert_eq!(state.output_tokens, Some(7));
    assert_eq!(state.cached_tokens, Some(5));
}

#[test]
fn apply_event_function_call_assembles_tool_call() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_a",
                "name": "shell",
            },
        }),
        &mut on_update,
    )
    .expect("ok");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"cmd\":\"ls\"}",
        }),
        &mut on_update,
    )
    .expect("ok");

    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected function tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_a");
    assert_eq!(call.name.as_str(), "shell");
    assert_eq!(
        call.arguments,
        tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Text("cmd".into()),
            tau_proto::CborValue::Text("ls".into())
        )])
    );
    assert_eq!(call.raw_arguments_json.as_deref(), Some("{\"cmd\":\"ls\"}"));
}

#[test]
fn apply_event_failed_returns_error() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": { "message": "model overloaded" },
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("response failed"));
            assert!(body.contains("model overloaded"));
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Streaming `error` event in the documented OpenAI Responses shape:
/// `{ type: "error", code: <code>, message: <msg> }` (no nested
/// `error` object). The retry classifier needs the code in the
/// `(type=...)` suffix to distinguish account caps from transport
/// hiccups.
#[test]
fn apply_event_error_top_level_code_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "code": "rate_limit_exceeded",
        "message": "Rate limit reached",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("Rate limit reached"));
            assert!(
                body.contains("(type=rate_limit_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
            assert!(
                crate::common::is_account_limit_body(&body),
                "is_account_limit_body must classify this body as a cap"
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Nested `error.code` shape — some Codex error envelopes wrap the
/// code in an `error` object alongside the message. Must produce the
/// same suffix as the top-level form.
#[test]
fn apply_event_error_nested_code_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "code": "usage_limit_reached",
            "message": "The usage limit has been reached",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("usage limit has been reached"));
            assert!(
                body.contains("(type=usage_limit_reached)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// Nested `error.type` shape observed from upstream — kept as a fallback so
/// captured provider events still classify correctly.
#[test]
fn apply_event_error_nested_type_fallback_is_propagated() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "type": "quota_exceeded",
            "message": "quota",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(
                body.contains("(type=quota_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

/// No code/type anywhere: body still produced, just without the
/// `(type=...)` suffix. The outer retry layer keeps retrying (we
/// can't safely classify), but we don't crash or drop the message.
#[test]
fn apply_event_error_without_code_omits_suffix() {
    let mut state = crate::common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "message": "something broke",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result {
        Err(LlmError::HttpStatus(0, body)) => {
            assert!(body.contains("something broke"));
            assert!(!body.contains("(type="), "unexpected suffix in {body:?}");
        }
        other => panic!("expected HttpStatus(0, ...), got {other:?}"),
    }
}

#[test]
fn repeated_output_text_delta_aborts_before_appending_more_output() {
    // Ensures the Responses stream guard aborts tight exact assistant text loops
    // before the repeated suffix can be emitted as a normal update.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.text.is_empty());
}

#[test]
fn repeated_tool_argument_delta_aborts_before_appending_more_arguments() {
    // Ensures tool-call argument streams use the same tight exact guard, because
    // argument loops can otherwise burn the provider output budget unseen.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_output_text_done_aborts_without_appending_snapshot() {
    // Done snapshots can carry all text without prior deltas; they must be guarded
    // before becoming assistant output.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.done",
        "output_index": 0,
        "text": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.text.is_empty());
}

#[test]
fn non_repeating_output_text_done_is_accepted() {
    // Non-repeating final snapshots are normal Responses events and must not be
    // rejected just because they bypassed delta streaming.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.done",
        "output_index": 0,
        "text": "This is a concise non-repeating answer.",
    });
    let done = apply_event(&mut state, &ev, &mut |_| {}).expect("done snapshot should apply");
    assert!(!done);
    assert_eq!(state.text, "This is a concise non-repeating answer.");
}

#[test]
fn repeated_function_arguments_done_aborts_without_appending_snapshot() {
    // Function argument done events can provide a full final argument string; the
    // guard must check it even when no argument deltas were sent.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.function_call_arguments.done",
        "output_index": 0,
        "arguments": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_custom_tool_input_done_aborts_without_appending_snapshot() {
    // Custom tool input done events share the same final-snapshot bypass risk as
    // function arguments.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.custom_tool_call_input.done",
        "output_index": 0,
        "input": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_output_item_done_message_aborts_without_appending_snapshot() {
    // Message output_item.done fallbacks are guarded in addition to the dedicated
    // output_text.done event.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "content": [{ "type": "output_text", "text": ".".repeat(1024) }]
        }
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.text.is_empty());
}

#[test]
fn repeated_output_item_done_tool_arguments_abort_without_appending_snapshot() {
    // Tool output_item.done fallbacks are guarded before final arguments are
    // accepted into the tool-call accumulator.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "call_id": "call-1",
            "name": "shell",
            "arguments": "_clone".repeat(180)
        }
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_reasoning_summary_delta_aborts_before_appending() {
    // Reasoning summaries are visible stream components and need the same tight
    // exact-loop protection as assistant text.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.reasoning_summary_text.delta",
        "output_index": 0,
        "delta": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.thinking.is_none());
}

#[test]
fn repeated_custom_tool_input_delta_aborts_before_appending() {
    // Custom tool input deltas are guarded independently from function arguments.
    let mut state = crate::common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.custom_tool_call_input.delta",
        "output_index": 0,
        "delta": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}
