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

/// The model-facing wait contract bounds only input waits, advertises the
/// silent cap without schema-rejecting larger values, and preserves `{}`.
#[test]
fn wait_spec_documents_optional_non_consuming_input_mode() {
    let spec = wait_tool_spec();
    let parameters = spec.parameters.expect("wait parameters");
    assert_eq!(
        parameters["properties"]["timeout_minutes"]["type"],
        serde_json::json!("integer")
    );
    assert_eq!(
        parameters["properties"]["timeout_minutes"]["minimum"],
        serde_json::json!(1)
    );
    assert!(
        parameters["properties"]["timeout_minutes"]
            .get("maximum")
            .is_none()
    );
    assert!(parameters["properties"].get("any_input").is_none());
    assert!(parameters.get("required").is_none());
    let description = spec.description.expect("wait description");
    assert!(description.contains("`wait({\"timeout_minutes\":N})`"));
    assert!(description.contains("cap values above 60"));
    assert!(description.contains("do not consume"));
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
    assert!(description.contains("delivered asynchronously via session-local `agent_watch`"));
    assert!(description.contains("until the caller disables the watch or the session ends"));
    assert!(
        description.contains("internal steering and tool-completion notices are not forwarded")
    );
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
    assert!(description.contains("session-local async notifications"));
    assert!(description.contains("automatically enables a watch"));
    assert!(description.contains("Watched agent <agent-id> emitted a response"));
    assert!(description.contains("internal steering and tool-completion notices"));
    assert!(description.contains("enable: false"));
    assert_eq!(
        params
            .pointer("/properties/agent_id/maxLength")
            .and_then(serde_json::Value::as_u64),
        Some(tau_proto::AGENT_ID_MAX_LEN as u64)
    );
    assert_eq!(
        params
            .pointer("/properties/agent_id/pattern")
            .and_then(serde_json::Value::as_str),
        Some("^[A-Za-z0-9_-]{1,64}$")
    );
    assert!(
        params
            .pointer("/properties/agent_id/description")
            .and_then(serde_json::Value::as_str)
            .expect("agent_id description")
            .contains("ASCII letters")
    );
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
    assert_eq!(parsed.agent_id.as_str(), "agent-a");
    assert!(parsed.enable);

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

    let err = parse_agent_watch_args(&CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("../agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(false)),
    ]))
    .expect_err("invalid agent_id should fail");
    assert!(err.starts_with("invalid `agent_id`:"));
}

/// Agent watch display args should make both the watch action and target agent
/// visible in the compact tool line, preventing ambiguous bare `agent_watch`
/// rows in the transcript.
#[test]
fn agent_watch_display_args_summarize_action_and_agent() {
    let enable = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]);
    let disable = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(false)),
    ]);

    assert_eq!(agent_watch_display_args(&enable), "watch agent-a");
    assert_eq!(agent_watch_display_args(&disable), "unwatch agent-a");
}

/// Invalid agent watch display args should still expose whitelisted safe fields
/// but must not echo arbitrary or unsafe strings from malformed tool calls.
#[test]
fn agent_watch_display_args_falls_back_to_safe_fields() {
    let missing_agent = CborValue::Map(vec![(
        CborValue::Text("enable".to_owned()),
        CborValue::Bool(true),
    )]);
    let missing_enable = CborValue::Map(vec![(
        CborValue::Text("agent_id".to_owned()),
        CborValue::Text("agent-a".to_owned()),
    )]);
    let unsafe_agent = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("../agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(false)),
    ]);
    let unsafe_enable = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (
            CborValue::Text("enable".to_owned()),
            CborValue::Text("false".to_owned()),
        ),
    ]);

    assert_eq!(agent_watch_display_args(&missing_agent), "watch");
    assert_eq!(agent_watch_display_args(&missing_enable), "agent-a");
    assert_eq!(agent_watch_display_args(&unsafe_agent), "unwatch");
    assert_eq!(agent_watch_display_args(&unsafe_enable), "agent-a");
}

/// Terminal agent watch results must carry the same bounded display args as the
/// in-progress row so compact history does not regress to a bare
/// `agent_watch ok` entry after completion.
#[test]
fn agent_watch_success_display_keeps_action_and_agent() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]);

    let display = agent_watch_success_display(&args);

    assert_eq!(display.args, "watch agent-a");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

/// Enabling or re-enabling through the actual tool-result formatter includes
/// the sanitized current snapshot exactly once and never model-input framing.
#[test]
fn agent_watch_enable_result_includes_safe_current_snapshot() {
    let result = agent_watch_enabled_result(
        "agent-a",
        Some("retrying (throttle, attempt 50, next retry about 60s)"),
    );
    assert_eq!(
        result,
        "Watching agent `agent-a`; current status: retrying (throttle, attempt 50, next retry about 60s)"
    );
    assert_eq!(result.matches("current status:").count(), 1);
    assert!(!result.contains("[tau-internal]"));
    assert_eq!(
        agent_watch_enabled_result("agent-a", None),
        "Watching agent `agent-a`"
    );
}

/// Terminal agent watch errors must also preserve sanitized display args while
/// keeping unsafe malformed identifiers out of the compact history row.
#[test]
fn agent_watch_error_display_keeps_safe_action() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-missing".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(false)),
    ]);

    let display = agent_watch_error_display(&args, "unknown agent: `agent-missing`");

    assert_eq!(display.args, "unwatch agent-missing");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "unknown agent: `agent-missing`");
}

#[derive(Default)]
struct RecordingAgentWatchFinisher {
    success: Option<RecordedAgentWatchSuccess>,
    error: Option<RecordedAgentWatchError>,
}

struct RecordedAgentWatchSuccess {
    conversation_id: AgentId,
    call_id: ToolCallId,
    tool_name: ToolName,
    tool_type: ToolType,
    result: CborValue,
    display: Option<ToolUseState>,
}

struct RecordedAgentWatchError {
    conversation_id: AgentId,
    call_id: ToolCallId,
    tool_name: ToolName,
    tool_type: ToolType,
    message: String,
    details: Option<CborValue>,
    display: Option<ToolUseState>,
}

impl AgentWatchFinisher for RecordingAgentWatchFinisher {
    fn finish_agent_watch_success(
        &mut self,
        conversation_id: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: CborValue,
        display: Option<ToolUseState>,
    ) {
        self.success = Some(RecordedAgentWatchSuccess {
            conversation_id: conversation_id.clone(),
            call_id,
            tool_name,
            tool_type,
            result,
            display,
        });
    }

    fn finish_agent_watch_error(
        &mut self,
        conversation_id: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
        display: Option<ToolUseState>,
    ) {
        self.error = Some(RecordedAgentWatchError {
            conversation_id: conversation_id.clone(),
            call_id,
            tool_name,
            tool_type,
            message,
            details,
            display,
        });
    }
}

/// Ensures the production success finisher path attaches final display
/// metadata, so the completed transcript row keeps the compact `watch <agent>`
/// summary.
#[test]
fn finish_agent_watch_success_passes_informative_display() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]);
    let mut finisher = RecordingAgentWatchFinisher::default();

    finish_agent_watch_success(
        &mut finisher,
        &AgentId::parse("parent-cid").expect("valid agent id"),
        ToolCallId::from("call-1"),
        ToolName::new(AGENT_WATCH_TOOL_NAME),
        ToolType::Function,
        &args,
        "Watching agent `agent-a`".to_owned(),
    );

    let call = finisher.success.expect("finish call recorded");
    let display = call.display.expect("informative display is attached");
    assert_eq!(call.conversation_id.as_str(), "parent-cid");
    assert_eq!(call.call_id.as_str(), "call-1");
    assert_eq!(call.tool_name.as_str(), AGENT_WATCH_TOOL_NAME);
    assert_eq!(call.tool_type, ToolType::Function);
    assert_eq!(
        call.result,
        CborValue::Text("Watching agent `agent-a`".to_owned())
    );
    assert_eq!(display.args, "watch agent-a");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

/// Ensures the production error finisher path attaches sanitized final display
/// metadata, preventing completed error rows from degrading to bare
/// `agent_watch` entries.
#[test]
fn finish_agent_watch_error_passes_informative_display() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-missing".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(false)),
    ]);
    let mut finisher = RecordingAgentWatchFinisher::default();

    finish_agent_watch_error(
        &mut finisher,
        &AgentId::parse("parent-cid").expect("valid agent id"),
        ToolCallId::from("call-1"),
        ToolName::new(AGENT_WATCH_TOOL_NAME),
        ToolType::Function,
        &args,
        "unknown agent: `agent-missing`".to_owned(),
    );

    let call = finisher.error.expect("finish call recorded");
    let display = call.display.expect("informative display is attached");
    assert_eq!(call.conversation_id.as_str(), "parent-cid");
    assert_eq!(call.call_id.as_str(), "call-1");
    assert_eq!(call.tool_name.as_str(), AGENT_WATCH_TOOL_NAME);
    assert_eq!(call.tool_type, ToolType::Function);
    assert_eq!(call.message, "unknown agent: `agent-missing`");
    assert_eq!(call.details, Some(args));
    assert_eq!(display.args, "unwatch agent-missing");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "unknown agent: `agent-missing`");
}

/// A stopped-target classification from the handler's atomic watch adapter
/// must finish as a tool error without reaching the adapter's mutation branch.
#[test]
fn agent_watch_stopped_target_errors_without_mutation() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("agent-stopped".to_owned()),
        ),
        (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
    ]);
    let parsed = parse_agent_watch_args(&args).expect("valid watch arguments");
    let mutation_called = std::cell::Cell::new(false);
    let result = agent_watch_tool_result("agent-watcher", &parsed, |_, watched_id, enable| {
        assert!(enable);
        if watched_id == "agent-stopped" {
            return Err(format!("agent is not live: `{watched_id}`"));
        }
        mutation_called.set(true);
        Ok(None)
    });
    let error = result.expect_err("stopped target must fail");
    let mut finisher = RecordingAgentWatchFinisher::default();
    finish_agent_watch_error(
        &mut finisher,
        &AgentId::parse("watcher-cid").expect("valid agent id"),
        ToolCallId::from("watch-stopped"),
        ToolName::new(AGENT_WATCH_TOOL_NAME),
        ToolType::Function,
        &args,
        error,
    );

    assert!(!mutation_called.get());
    assert!(finisher.success.is_none());
    let call = finisher.error.expect("tool error recorded");
    assert_eq!(call.call_id.as_str(), "watch-stopped");
    assert_eq!(call.message, "agent is not live: `agent-stopped`");
    assert_eq!(
        call.display.expect("error display").status,
        ToolUseStatus::Error
    );
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
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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

/// Internal or steering-originated provider turns, such as background tool
/// completion wakeups, must not be forwarded to watchers. Watchers should only
/// see final watched-agent responses that belong to an interactive user turn.
#[test]
fn agent_watch_ignores_internal_originated_responses() {
    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-watch".into(),
        agent_id: tau_proto::AgentId::parse("agent-a").expect("agent id"),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "background completion handled".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::new("__harness__"),
            query_id: "background-completion".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    assert!(agent_watch_response_should_notify(&response).is_none());
}

/// Untyped provider errors are represented by the harness's structured unknown
/// terminal status and must not also produce a legacy watch response containing
/// provider-authored diagnostics.
#[test]
fn agent_watch_legacy_response_suppresses_untyped_provider_errors() {
    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-watch-error".into(),
        agent_id: tau_proto::AgentId::parse("agent-a").expect("agent id"),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("secret raw endpoint body".to_owned()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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

/// A completed `agent_start` result is the child agent's terminal answer to the
/// watcher that delegated it. That final result remains watchable even though
/// non-user per-turn provider responses are filtered elsewhere.
#[test]
fn agent_watch_forwards_terminal_agent_start_result() {
    let result = tau_proto::StartAgentResult {
        query_id: "delegate-1".to_owned(),
        text: "delegated final".to_owned(),
        error: None,
    };

    assert_eq!(
        start_agent_watch_notification_message("agent-a", &result),
        Some("delegated final".to_owned())
    );
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
        MessageRecipientAddress::External {
            session_id,
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(agent_id),
        } => {
            assert_eq!(session_id.as_str(), "session-b");
            assert_eq!(agent_id.as_str(), "agent_b");
        }
        _ => panic!("expected external recipient"),
    }
    assert!(parse_message_recipient("session-b/agent/extra", &current).is_err());
    assert!(parse_message_recipient("session-b/", &current).is_err());
    assert!(parse_message_recipient("session-b/bad/agent", &current).is_err());
    assert!(matches!(
        parse_message_recipient("&session-b", &current),
        Ok(MessageRecipientAddress::External {
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            ..
        })
    ));
    assert!(matches!(
        parse_message_recipient("&session-b/@agent_b", &current),
        Ok(MessageRecipientAddress::External {
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(agent),
            ..
        }) if agent.as_str() == "agent_b"
    ));
    assert!(matches!(
        parse_message_recipient("&session-a", &current),
        Ok(MessageRecipientAddress::LocalEntrypoint)
    ));
    assert!(parse_message_recipient("&session-b/agent_b", &current).is_err());
    assert!(parse_message_recipient("&bad session", &current).is_err());
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

    state.record_runtime_bookkeeping_event(&Event::ProviderToolResult(tool_result(
        "shell-call",
        ToolResultKind::BackgroundPlaceholder,
    )));
    let display = state
        .initial_display(&wait_call("shell-call"))
        .expect("wait display after placeholder");
    assert_eq!(display.args, "shell");

    state.record_runtime_bookkeeping_event(&Event::ToolBackgroundResult(tool_background_result(
        "shell-call",
    )));
    let display = state
        .initial_display(&wait_call("shell-call"))
        .expect("wait display after finish");
    assert_eq!(display.args, "");
}

/// Ensures cancellation bookkeeping follows the target tool lifecycle instead
/// of keeping stale ids forever, so a completed target is not later reported as
/// "already canceled" and the in-memory set stays bounded.
#[test]
fn cancel_request_tracking_is_cleared_when_target_finishes() {
    let mut state = BuiltinState::default();
    let call_id = ToolCallId::from("shell-call");
    state.cancel_requested.insert(call_id.clone());

    state.record_runtime_bookkeeping_event(&Event::ToolBackgroundResult(tool_background_result(
        call_id.as_str(),
    )));

    assert!(!state.cancel_requested.contains(&call_id));
}

/// Ensures duplicate-cancel bookkeeping is checked only after owner-scoped
/// target validation. Otherwise another conversation could learn that a guessed
/// call id is real and already has a pending cancellation request.
#[test]
fn cancel_duplicate_request_state_does_not_leak_to_non_owner() {
    let mut state = BuiltinState::default();
    let call_id = ToolCallId::from("owned-call");
    state.cancel_requested.insert(call_id.clone());

    let err = state
        .validate_cancel_request(&call_id, false, false)
        .expect_err("non-owner should see unknown call");
    assert_eq!(err, "Unknown tool call id");

    let err = state
        .validate_cancel_request(&call_id, true, false)
        .expect_err("owner should still see duplicate cancel");
    assert_eq!(err, "Tool call already canceled");
}

/// Ensures completed-call diagnostics are likewise emitted only for the owning
/// conversation; non-owners get the same unknown-id response as absent calls.
#[test]
fn cancel_completed_request_state_is_owner_scoped() {
    let mut state = BuiltinState::default();
    let call_id = ToolCallId::from("completed-call");

    let err = state
        .validate_cancel_request(&call_id, false, false)
        .expect_err("non-owner should see unknown call");
    assert_eq!(err, "Unknown tool call id");

    let err = state
        .validate_cancel_request(&call_id, false, true)
        .expect_err("owner should see completed call");
    assert_eq!(err, "Tool call is already done");
}

/// Ensures session shutdown drops runtime-only bookkeeping for abandoned
/// in-flight tools and pending delegate routing. Watch relationships now live
/// in harness session state and are covered by harness-level cleanup tests.
#[test]
fn session_runtime_state_cleanup_clears_in_flight_bookkeeping() {
    let mut state = BuiltinState::default();
    let call_id = ToolCallId::from("shell-call");
    state.cancel_requested.insert(call_id.clone());
    state.record_tool_started(call_id.clone(), ToolName::new("shell"));
    state.pending_delegates.insert(
        "delegate-1".to_owned(),
        PendingDelegate {
            agent_id: "agent-child".to_owned(),
        },
    );

    state.record_runtime_bookkeeping_event(&Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: "session-next".into(),
    }));

    assert!(state.cancel_requested.is_empty());
    assert!(state.in_progress_tool_names.is_empty());
    assert!(state.pending_delegates.is_empty());
}

/// Ensures whitespace-only cancellation targets are rejected as empty rather
/// than being published as impossible tool ids.
#[test]
fn cancel_args_reject_whitespace_only_tool_call_id() {
    let args = CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Text(" \n\t ".to_owned()),
    )]);

    let err = parse_cancel_args(&args).expect_err("whitespace id should fail");

    assert_eq!(err, "`tool_call_id` must not be empty");
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
    let value = delegate_result_value("engineer_parent", "engineer_child");

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

/// Ensures the built-in `agent_start` tool's immediate success descriptor
/// contains the started agent id and prompt-size metadata that replaced the old
/// long-running delegate progress line.
#[test]
fn agent_start_success_display_names_agent_and_uses_standard_status() {
    // Immediate `agent_start` completion no longer carries the child's final
    // answer, so its display descriptor must still give the user useful spawn
    // metadata in the normal tool-call block.
    let display = agent_start_success_display(
        "review",
        "engineer_child",
        ToolUseStats {
            matches: None,
            lines: Some(2),
            bytes: Some(12),
        },
    );

    assert_eq!(display.args, "[review]");
    assert_eq!(display.stats.lines, Some(2));
    assert_eq!(display.stats.bytes, Some(12));
    assert_eq!(display.info_chips, vec!["@engineer_child"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[derive(Default)]
struct RecordingAgentStartFinisher {
    call: Option<RecordedAgentStartFinish>,
}

struct RecordedAgentStartFinish {
    conversation_id: AgentId,
    call_id: ToolCallId,
    tool_name: ToolName,
    tool_type: ToolType,
    result: CborValue,
    display: Option<ToolUseState>,
}

impl AgentStartSuccessFinisher for RecordingAgentStartFinisher {
    fn finish_agent_start_success(
        &mut self,
        conversation_id: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: CborValue,
        display: Option<ToolUseState>,
    ) {
        self.call = Some(RecordedAgentStartFinish {
            conversation_id: conversation_id.clone(),
            call_id,
            tool_name,
            tool_type,
            result,
            display,
        });
    }
}

/// Ensures the production completion helper wires the informative display into
/// the actual finish call, preventing regressions where `agent_start` silently
/// returns to an unadorned `None` display.
#[test]
fn finish_agent_start_success_passes_informative_display() {
    let mut finisher = RecordingAgentStartFinisher::default();
    finish_agent_start_success(
        &mut finisher,
        &AgentId::parse("parent-cid").expect("valid agent id"),
        ToolCallId::from("call-1"),
        ToolName::new(AGENT_START_TOOL_NAME),
        ToolType::Function,
        AgentStartSuccess {
            self_agent_id: "engineer_parent",
            agent_id: "engineer_child",
            task_name: "review",
            prompt_stats: ToolUseStats {
                matches: None,
                lines: Some(2),
                bytes: Some(12),
            },
        },
    );
    let call = finisher.call.expect("finish call recorded");
    let display = call.display.expect("informative display is attached");

    assert_eq!(call.conversation_id.as_str(), "parent-cid");
    assert_eq!(call.call_id.as_str(), "call-1");
    assert_eq!(call.tool_name.as_str(), AGENT_START_TOOL_NAME);
    assert_eq!(call.tool_type, ToolType::Function);
    assert_eq!(
        cbor_map_text(&call.result, "self_agent_id"),
        Some("engineer_parent")
    );
    assert_eq!(
        cbor_map_text(&call.result, "sub_agent_id"),
        Some("engineer_child")
    );
    assert_eq!(display.args, "[review]");
    assert_eq!(display.stats.lines, Some(2));
    assert_eq!(display.stats.bytes, Some(12));
    assert_eq!(display.info_chips, vec!["@engineer_child"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
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

/// The model-visible watch contract must distinguish client-only initial state
/// from later transition prompts so agents do not act on the enable snapshot.
#[test]
fn agent_watch_spec_documents_initial_and_transition_context_semantics() {
    let description = agent_watch_tool_spec()
        .description
        .expect("agent_watch description");

    assert!(description.contains("client-visible current model-turn state"));
    assert!(description.contains("initial status is not injected"));
    assert!(description.contains("later transitions are delivered separately"));
}

/// Ensures both compaction capabilities remain absent unless policy explicitly
/// opts in, and that each advertises instant-background completion semantics.
#[test]
fn compaction_specs_are_independently_disabled_and_backgrounded() {
    let compact = compact_tool_spec();
    let cross = agent_compact_tool_spec();

    assert!(!compact.enabled_by_default);
    assert!(!cross.enabled_by_default);
    assert_eq!(
        compact.background_support,
        Some(tau_proto::BackgroundSupport::Instant)
    );
    assert_eq!(
        cross.background_support,
        Some(tau_proto::BackgroundSupport::Instant)
    );
    assert_eq!(
        compact.tags,
        vec![
            tau_proto::ToolTag::new("harness:compaction"),
            tau_proto::ToolTag::new("harness:compaction:self")
        ]
    );
    assert!(
        cross
            .tags
            .contains(&tau_proto::ToolTag::new("harness:agent-control"))
    );
    assert!(
        !compact
            .tags
            .iter()
            .any(|tag| tag.as_str().contains("cross"))
    );
}

/// Ensures the self tool accepts only the empty object so a provider cannot
/// smuggle target authority through unknown arguments.
#[test]
fn compact_arguments_are_strictly_empty() {
    assert_eq!(
        parse_compaction_args(COMPACT_TOOL_NAME, &CborValue::Map(Vec::new())),
        Ok(None)
    );
    assert!(
        parse_compaction_args(
            COMPACT_TOOL_NAME,
            &CborValue::Map(vec![(
                CborValue::Text("agent_id".to_owned()),
                CborValue::Text("other".to_owned())
            )])
        )
        .is_err()
    );
}

/// Ensures cross-agent authority requires one syntactically safe non-empty
/// target and rejects every extra field.
#[test]
fn agent_compact_arguments_require_only_agent_id() {
    let valid = CborValue::Map(vec![(
        CborValue::Text("agent_id".to_owned()),
        CborValue::Text("other-agent".to_owned()),
    )]);
    assert_eq!(
        parse_compaction_args(AGENT_COMPACT_TOOL_NAME, &valid),
        Ok(Some(
            tau_proto::AgentId::parse("other-agent").expect("valid agent id")
        ))
    );
    assert!(parse_compaction_args(AGENT_COMPACT_TOOL_NAME, &CborValue::Map(Vec::new())).is_err());
    let extra = CborValue::Map(vec![
        (
            CborValue::Text("agent_id".to_owned()),
            CborValue::Text("other-agent".to_owned()),
        ),
        (CborValue::Text("force".to_owned()), CborValue::Bool(true)),
    ]);
    assert!(parse_compaction_args(AGENT_COMPACT_TOOL_NAME, &extra).is_err());
}

/// Discovery capabilities are independently opt-in and never become available
/// merely because the built-in handler is installed.
#[test]
fn discovery_tools_are_disabled_by_default_in_separate_groups() {
    let tools = BuiltinTools::default();
    let specs = tools.tool_specs();
    for (tool_name, group_name) in [
        (SESSION_LIST_TOOL_NAME, "session_discovery"),
        (AGENT_LIST_TOOL_NAME, "agent_discovery"),
    ] {
        let spec = specs
            .iter()
            .find(|spec| spec.name.as_str() == tool_name)
            .expect("discovery tool spec");
        assert!(!spec.enabled_by_default);
        assert_eq!(
            tools
                .tool_group(&spec.name)
                .expect("discovery group")
                .name
                .as_str(),
            group_name
        );
    }
}

/// Bounded discovery parsing rejects unknown filters, invalid states, and
/// non-positive limits instead of silently broadening enumeration.
#[test]
fn discovery_arguments_are_strict_and_bounded() {
    let valid = CborValue::Map(vec![
        (
            CborValue::Text("query".to_owned()),
            CborValue::Text("peer".to_owned()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(500_u64.into()),
        ),
    ]);
    let parsed = parse_discovery_args(&valid, false).expect("valid session filters");
    assert_eq!(parsed.query.as_deref(), Some("peer"));
    assert_eq!(parsed.limit, DISCOVERY_MAX_RESULTS);
    assert!(
        parse_discovery_args(
            &CborValue::Map(vec![(
                CborValue::Text("state".to_owned()),
                CborValue::Text("sleeping".to_owned()),
            )]),
            true,
        )
        .is_err()
    );
    assert!(
        parse_discovery_args(
            &CborValue::Map(vec![(
                CborValue::Text("agent_id".to_owned()),
                CborValue::Text("secret".to_owned()),
            )]),
            true,
        )
        .is_err()
    );
    for filter in ["query", "role", "group", "state"] {
        assert!(
            parse_discovery_args(
                &CborValue::Map(vec![(
                    CborValue::Text(filter.to_owned()),
                    CborValue::Text("x".repeat(257)),
                )]),
                true,
            )
            .is_err(),
            "{filter} must have a byte bound"
        );
    }
}
