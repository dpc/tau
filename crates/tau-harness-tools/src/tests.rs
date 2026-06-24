use super::*;

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
    })
}

fn cbor_map_bool(value: &CborValue, key: &str) -> Option<bool> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Bool(value) => Some(*value),
                _ => None,
            })
    })
}
fn wait_args_exact(call_id: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Text(call_id.to_owned()),
    )])
}

fn wait_call(target_call_id: &str) -> AgentToolCall {
    AgentToolCall {
        id: "wait-call".into(),
        name: ToolName::new(WAIT_TOOL_NAME),
        tool_type: ToolType::Function,
        arguments: wait_args_exact(target_call_id),
    }
}

fn message_call(recipient_id: &str, message: &str) -> AgentToolCall {
    AgentToolCall {
        id: "message-call".into(),
        name: ToolName::new(MESSAGE_TOOL_NAME),
        tool_type: ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("recipient_id".to_owned()),
                CborValue::Text(recipient_id.to_owned()),
            ),
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text(message.to_owned()),
            ),
        ]),
    }
}

fn tool_result(call_id: &str, kind: ToolResultKind) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("shell"),
        tool_type: ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        kind,
        display: None,
        originator: PromptOriginator::User,
    }
}

fn tool_background_result(call_id: &str) -> tau_proto::ToolBackgroundResult {
    tau_proto::ToolBackgroundResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("shell"),
        tool_type: ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        display: None,
        originator: PromptOriginator::User,
    }
}

#[test]
fn agent_start_spec_advertises_only_current_tool_name() {
    // Tau does not preserve compatibility for renamed tool call names yet.
    // Only the current public spelling should be advertised or handled.
    let tools = BuiltinTools::default();
    let names: Vec<String> = tools
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();

    assert!(names.iter().any(|name| name == AGENT_START_TOOL_NAME));
    assert!(!names.iter().any(|name| name == "delegate"));
    assert!(tools.handles(&ToolName::new(AGENT_START_TOOL_NAME)));
    assert!(!tools.handles(&ToolName::new("delegate")));
    let description = agent_start_tool_spec()
        .description
        .expect("agent_start description");
    assert!(description.contains("delivered asynchronously via `agent_watch`"));
    assert!(description.contains("until the caller disables the watch"));
    assert!(description.contains("metadata"));
    assert!(!description.contains("return its final response"));
}

#[test]
fn agent_watch_spec_is_advertised_and_requires_agent_id_and_enable() {
    let spec = agent_watch_tool_spec();
    assert_eq!(spec.name.as_str(), AGENT_WATCH_TOOL_NAME);
    let params = spec.parameters.expect("agent_watch schema");
    let required = params
        .get("required")
        .and_then(serde_json::Value::as_array)
        .expect("required fields")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();

    let description = agent_watch_tool_spec()
        .description
        .expect("agent_watch description");
    assert!(description.contains("persistent async notifications"));
    assert!(description.contains("automatically enables a watch"));
    assert!(description.contains("enable: false"));
    assert_eq!(required, vec!["agent_id", "enable"]);
}

#[test]
fn agent_watch_args_require_non_empty_agent_id_and_bool_enable() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]);
    let parsed = parse_agent_watch_args(&args).expect("valid watch args");
    assert_eq!(parsed.agent_id, "agent-a");
    assert!(parsed.enable);
    assert_eq!(agent_watch_display_args(&parsed), "agent-a on");

    let err = parse_agent_watch_args(&CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]))
    .expect_err("empty agent_id should fail");
    assert_eq!(err, "`agent_id` must not be empty");

    let err = parse_agent_watch_args(&CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (
            CborValue::Text("enable".to_owned()),
            CborValue::Text("true".to_owned()),
        ),
    ]))
    .expect_err("non-bool enable should fail");
    assert_eq!(err, "`enable` must be a boolean");
}

#[test]
fn agent_watch_notification_extracts_assistant_response_text() {
    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-watch".into(),
        agent_id: tau_proto::AgentId::parse("agent-a").expect("agent id"),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    assert_eq!(
        agent_watch_notification_message(&response),
        Some("done".to_owned())
    );
}

#[test]
fn agent_watch_ignores_mid_turn_tool_call_responses() {
    // Tool-call stops are mid-turn: the final response is only known after the
    // requested tools run and the provider completes a later turn.
    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-watch".into(),
        agent_id: tau_proto::AgentId::parse("agent-a").expect("agent id"),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "working".to_owned(),
            }],
            phase: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    assert!(agent_watch_response_should_notify(&response).is_none());
}

/// Ensures the message tool treats `user`, bare agent ids, and
/// `<current-session>/<agent>` as local recipients so existing local workflows
/// are not forced through the external socket path.
#[test]
fn message_recipient_parser_recognizes_user_local_and_current_session() {
    let current: tau_proto::SessionId = "session-a".into();

    assert!(matches!(
        parse_message_recipient("user", &current),
        Ok(MessageRecipientAddress::User)
    ));
    assert!(matches!(
        parse_message_recipient("agent_a", &current),
        Ok(MessageRecipientAddress::LocalAgent(agent)) if agent.as_str() == "agent_a"
    ));
    assert!(matches!(
        parse_message_recipient("session-a/agent_b", &current),
        Ok(MessageRecipientAddress::LocalAgent(agent)) if agent.as_str() == "agent_b"
    ));
}

/// Ensures external message addresses require exactly one slash and a valid
/// right-hand agent id, preventing ambiguous `session/agent/extra` parsing.
#[test]
fn message_recipient_parser_validates_external_address_grammar() {
    let current: tau_proto::SessionId = "session-a".into();

    match parse_message_recipient("session-b/agent_b", &current).expect("valid external recipient")
    {
        MessageRecipientAddress::ExternalAgent {
            session_id,
            agent_id,
        } => {
            assert_eq!(session_id.as_str(), "session-b");
            assert_eq!(agent_id.as_str(), "agent_b");
        }
        _ => panic!("expected external recipient"),
    }
    assert!(parse_message_recipient("session-b/agent/extra", &current).is_err());
    assert!(parse_message_recipient("session-b/", &current).is_err());
    assert!(parse_message_recipient("session-b/bad/agent", &current).is_err());
}

#[test]
fn wait_initial_display_uses_tracked_target_tool_name() {
    // Regression for provider-owned running display: the wait tool should
    // show the logical source tool name, not the opaque target call id.
    let mut state = BuiltinState::default();
    state.record_tool_started("shell-call".into(), ToolName::new("shell"));

    let display = state
        .initial_display(&wait_call("shell-call"))
        .expect("wait display");

    assert_eq!(display.args, "shell");
    assert_eq!(display.status, ToolUseStatus::InProgress);
}

/// The message tool progress display must keep the recipient inline and put
/// the actual delivered text in the rich payload, so UIs can show it even when
/// the separate message event scrolls by.
#[test]
fn message_initial_display_includes_message_payload() {
    let state = BuiltinState::default();

    let display = state
        .initial_display(&message_call("user", "please check this"))
        .expect("message display");

    assert_eq!(display.args, "user");
    assert_eq!(display.status, ToolUseStatus::InProgress);
    assert_eq!(
        display.payload,
        Some(ToolUsePayload::Text {
            text: "please check this".to_owned(),
        })
    );
}

#[test]
fn wait_initial_display_tracks_only_running_or_backgrounded_tools() {
    let mut state = BuiltinState::default();
    state.record_tool_started("shell-call".into(), ToolName::new("shell"));

    state.record_tool_lifecycle_event(&Event::ProviderToolResult(tool_result(
        "shell-call",
        ToolResultKind::BackgroundPlaceholder,
    )));
    let display = state
        .initial_display(&wait_call("shell-call"))
        .expect("wait display after placeholder");
    assert_eq!(display.args, "shell");

    state.record_tool_lifecycle_event(&Event::ToolBackgroundResult(tool_background_result(
        "shell-call",
    )));
    let display = state
        .initial_display(&wait_call("shell-call"))
        .expect("wait display after finish");
    assert_eq!(display.args, "");
}

#[test]
fn delegate_instruction_names_parent_and_message_followup_path() {
    // Delegated agents get a fresh context, so their injected instruction
    // must explicitly name the parent and explain that responses flow back
    // through the unified agent_watch notification path while the watch is enabled.
    let instruction = delegate_instruction("engineer_parent", "inspect the change");

    assert!(
        instruction
            .contains("You were started by agent `engineer_parent` using `agent_start` tool")
    );
    assert!(instruction.contains("automatically watching this conversation"));
    assert!(instruction.contains("async `agent_watch` notifications"));
    assert!(instruction.contains("while that watch remains enabled"));
    assert!(instruction.contains("the `message` tool to communicate with any agent at any time"));
    assert!(instruction.contains("### Task\n\ninspect the change"));
}

#[test]
fn delegate_result_includes_only_caller_and_sub_agent_ids() {
    // `agent_start` no longer returns the sub-agent's first final text as tool
    // output. That content is delivered through the unified `agent_watch`
    // notification path, while the tool result keeps routing metadata.
    let value = delegate_result_value(None, None, Some("engineer_parent"), Some("engineer_child"));

    assert_eq!(
        cbor_map_text(&value, "self_agent_id"),
        Some("engineer_parent")
    );
    assert_eq!(
        cbor_map_text(&value, "sub_agent_id"),
        Some("engineer_child")
    );
    assert_eq!(cbor_map_text(&value, "agent_id"), None);
    assert_eq!(cbor_map_text(&value, "output"), None);
}

#[test]
fn skill_search_guidance_omits_content_hint_when_content_was_already_searched() {
    let (result, _) = skill_search_result(
        &["missing".to_owned()],
        true,
        SkillSearchOutcome {
            hits: Vec::new(),
            total_matches: 0,
            truncated: false,
            auto_load_name: None,
            warnings: Vec::new(),
        },
    );

    assert_eq!(cbor_map_bool(&result, "search_content"), Some(true));
    let guidance = cbor_map_text(&result, "guidance").expect("guidance");
    assert!(guidance.contains("No skills matched"));
    assert!(!guidance.contains("search_content: true"));
}

#[test]
fn skill_search_guidance_suggests_content_search_only_when_not_already_enabled() {
    let (result, _) = skill_search_result(
        &["missing".to_owned()],
        false,
        SkillSearchOutcome {
            hits: Vec::new(),
            total_matches: 0,
            truncated: false,
            auto_load_name: None,
            warnings: Vec::new(),
        },
    );

    let guidance = cbor_map_text(&result, "guidance").expect("guidance");
    assert!(guidance.contains("search_content: true"));
}

#[test]
fn skill_query_rejects_whitespace_without_echoing_raw_input() {
    let args = CborValue::Map(vec![(
        CborValue::Text("query".to_owned()),
        CborValue::Text("  \n\t  ".to_owned()),
    )]);

    let err = extract_skill_search_queries(&args).expect_err("whitespace query should fail");

    assert_eq!(err, "query must include at least one non-empty term");
    assert!(!err.contains('\n'));
    assert!(!err.contains('\t'));
}
