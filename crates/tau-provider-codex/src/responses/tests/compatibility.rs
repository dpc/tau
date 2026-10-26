use super::*;

fn compatibility_request_snapshot(mode: ResponsesMode) -> serde_json::Value {
    let config = crate::config_for_model_mode(
        &tau_proto::ModelName::new("gpt-5.6-sol"),
        "compat-access-token".to_owned(),
        Some("acct-compat".to_owned()),
        mode,
    );
    let tools = [
        tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("lookup"),
            model_visible_name: None,
            description: Some("Look up one value.".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"],
                "additionalProperties": false
            })),
            format: None,
        },
        tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("code"),
            model_visible_name: None,
            description: Some("Run a code snippet.".to_owned()),
            tool_type: tau_proto::ToolType::Custom,
            parameters: None,
            format: Some(tau_proto::ToolFormat::Text),
        },
    ];
    let request = PromptPayload {
        system_prompt: "Compatibility system instructions.",
        context: context(&[
            user_text("legacy user input"),
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "legacy assistant output".to_owned(),
                }],
                phase: Some(tau_proto::MessagePhase::FinalAnswer),
                responses_raw_json: None,
            }),
        ]),
        hosted_tools: &[],
        tools: &tools,
        params: tau_proto::ModelParams {
            effort: tau_proto::ReasoningSelection::native(tau_proto::NativeReasoningEffort::High),
            thinking_summary: tau_proto::ThinkingSummary::Detailed,
            verbosity: tau_proto::Verbosity::Low,
            service_tier: Some(tau_proto::ServiceTier::Fast),
        },
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("compat-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("compat-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    serde_json::to_value(build_request(&config, &request, None))
        .expect("serialize compatibility Responses request")
}

/// Builds the retired unary compact request retained as historical wire
/// compatibility evidence.
fn compatibility_compact_request_snapshot(mode: ResponsesMode) -> serde_json::Value {
    compact_request_snapshot(mode, false)
}

/// Builds synthetic compact history for either the historical unary fixture or
/// the production fresh WebSocket envelope, without replacing historical
/// evidence.
fn compact_request_snapshot(mode: ResponsesMode, production: bool) -> serde_json::Value {
    let config = crate::config_for_model_mode(
        &tau_proto::ModelName::new("gpt-5.6-sol"),
        "compat-access-token".to_owned(),
        Some("acct-compat".to_owned()),
        mode,
    );
    let tools = [
        tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("lookup"),
            model_visible_name: None,
            description: Some("Look up one value.".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"],
                "additionalProperties": false
            })),
            format: None,
        },
        tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("code"),
            model_visible_name: None,
            description: Some("Run a code snippet.".to_owned()),
            tool_type: tau_proto::ToolType::Custom,
            parameters: None,
            format: Some(tau_proto::ToolFormat::Text),
        },
    ];
    let request = PromptPayload {
        system_prompt: "Compatibility system instructions.",
        context: context(&[
            user_text("legacy user input"),
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "legacy assistant output".to_owned(),
                }],
                phase: Some(tau_proto::MessagePhase::FinalAnswer),
                responses_raw_json: None,
            }),
            ContextItem::CompactionTrigger,
        ]),
        hosted_tools: &[],
        tools: &tools,
        params: tau_proto::ModelParams {
            effort: tau_proto::ReasoningSelection::native(tau_proto::NativeReasoningEffort::High),
            thinking_summary: tau_proto::ThinkingSummary::Detailed,
            verbosity: tau_proto::Verbosity::Low,
            service_tier: Some(tau_proto::ServiceTier::Fast),
        },
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("compat-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("compat-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    if production {
        let mut context = request.context.clone();
        let tau_proto::ContextBlock::UserInput(block) = &mut context.blocks[0] else {
            unreachable!()
        };
        block.items.pop();
        let assistant = block.items.pop().expect("assistant history");
        block.items.insert(
            0,
            ContextItem::Compaction(
                OpaqueProviderItem::from_raw_json(
                    r#"{"type":"compaction","id":"cmp_previous","encrypted_content":"synthetic-opaque"}"#,
                )
                .expect("synthetic opaque replacement"),
            ),
        );
        context
            .blocks
            .push(tau_proto::ContextBlock::AssistantResponse(
                tau_proto::AssistantResponseBlock {
                    provider_response_id: None,
                    backend: None,
                    output_items: vec![assistant],
                    usage: None,
                },
            ));
        context.blocks.push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            },
        ));
        tau_provider::local_summary_compaction::validate_trailing_trigger(&context)
            .expect("production compact context has one separate trailing trigger");
        let request = PromptPayload {
            context: &context,
            ..request
        };
        serde_json::to_value(build_ws_envelope(&config, &request, None, None))
            .expect("serialize production compact envelope")
    } else {
        build_compact_request(&config, &request).expect("build compatibility compact request")
    }
}

/// Assert that a frozen private Codex wire fixture cannot acquire public
/// OpenAI cache-control fields through a shared public-provider refactor.
fn assert_private_fixture_owns_cache_policy(fixture: &serde_json::Value) {
    let object = fixture.as_object().expect("private request fixture object");
    assert!(
        object.contains_key("prompt_cache_key"),
        "private fixture must retain its stable provider-owned cache identity"
    );
    assert!(
        !object.contains_key("prompt_cache_options")
            && !object.contains_key("prompt_cache_retention"),
        "private fixture must not acquire public cache controls"
    );
}

/// Standard and explicit Lite request lowering are frozen as complete JSON
/// goldens before transport and ownership changes begin.
#[test]
fn standard_and_lite_request_goldens() {
    for (mode, fixture) in [
        (ResponsesMode::Standard, "responses-standard.json"),
        (ResponsesMode::LiteCompatibility, "responses-lite.json"),
    ] {
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/fixtures/compat/{fixture}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap_or_else(|error| panic!("read Responses golden {fixture}: {error}")),
        )
        .unwrap_or_else(|error| panic!("decode Responses golden {fixture}: {error}"));
        assert_private_fixture_owns_cache_policy(&expected);
        let actual = compatibility_request_snapshot(mode);
        assert_eq!(
            actual,
            expected,
            "Responses request golden changed ({fixture}):\n{}",
            serde_json::to_string_pretty(&actual).expect("render actual request")
        );
    }
}

/// Standard and Lite compact requests retain only reviewed compact-schema
/// members and lower required tools into canonical input items.
#[test]
fn standard_and_lite_compact_request_goldens() {
    for (mode, fixture) in [
        (ResponsesMode::Standard, "responses-compact-standard.json"),
        (
            ResponsesMode::LiteCompatibility,
            "responses-compact-lite.json",
        ),
    ] {
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/fixtures/compat/{fixture}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap_or_else(|error| panic!("read compact Responses golden {fixture}: {error}")),
        )
        .unwrap_or_else(|error| panic!("decode compact Responses golden {fixture}: {error}"));
        let actual = compatibility_compact_request_snapshot(mode);
        assert_eq!(
            actual,
            expected,
            "compact Responses request golden changed ({fixture}):\n{}",
            serde_json::to_string_pretty(&actual).expect("render actual compact request")
        );
    }
}

/// Freeze the actual production response.create envelope in both modes,
/// including opaque history, tools, routing, and one final trigger on a fresh
/// full window.
#[test]
fn standard_and_lite_production_compact_request_goldens() {
    for (mode, fixture) in [
        (
            ResponsesMode::Standard,
            "responses-compact-ws-standard.json",
        ),
        (
            ResponsesMode::LiteCompatibility,
            "responses-compact-ws-lite.json",
        ),
    ] {
        let path = format!("{}/fixtures/compat/{fixture}", env!("CARGO_MANIFEST_DIR"));
        let actual = compact_request_snapshot(mode, true);
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("read production compact golden"),
        )
        .expect("decode production compact golden");
        assert_private_fixture_owns_cache_policy(&expected);
        assert_eq!(
            actual,
            expected,
            "production compact golden changed ({fixture}):\n{}",
            serde_json::to_string_pretty(&actual).expect("render production envelope")
        );
    }
}
