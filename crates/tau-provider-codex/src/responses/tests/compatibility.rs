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
        tools: &tools,
        params: tau_proto::ModelParams {
            effort: tau_proto::Effort::High,
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
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    serde_json::to_value(build_request(&config, &request, None))
        .expect("serialize compatibility Responses request")
}

/// Builds the documented unary compact request from the same complete
/// compatibility input as ordinary inference lowering.
fn compatibility_compact_request_snapshot(mode: ResponsesMode) -> serde_json::Value {
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
        tools: &tools,
        params: tau_proto::ModelParams {
            effort: tau_proto::Effort::High,
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
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    build_compact_request(&config, &request).expect("build compatibility compact request")
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
