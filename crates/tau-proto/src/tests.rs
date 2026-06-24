use super::*;

/// Ensures older serialized `extension.skill_available` payloads remain
/// readable and default to user-invocable/model-invocable behavior.
#[test]
fn ext_skill_available_serde_defaults_new_invocation_fields() {
    let value = serde_json::json!({
        "name": "legacy-skill",
        "description": "Legacy skill",
        "file_path": "/tmp/legacy/SKILL.md",
        "add_to_prompt": false
    });
    let skill: ExtSkillAvailable = serde_json::from_value(value).expect("legacy skill event");
    assert!(skill.user_invocable);
    assert!(!skill.disable_model_invocation);
    assert!(skill.argument_hint.is_none());
}

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("test agent id")
}

fn user_text_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })
}

fn action_schema_fixture() -> ActionSchema {
    ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: "/email".to_owned(),
            description: "Review email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![ActionCommand {
                name: "out".to_owned(),
                description: "Outgoing approvals".to_owned(),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "list".to_owned(),
                    description: "List queued outgoing email".to_owned(),
                    action_id: Some("email.out.list".to_owned()),
                    args: Vec::new(),
                    children: Vec::new(),
                }],
            }],
        }],
    }
}

fn representative_events() -> Vec<Event> {
    vec![
        Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo a payload".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        }),
        Event::ToolRequest(ToolRequest {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            arguments: CborValue::Text("hello".to_owned()),
            agent_id: agent_id("agent-1"),
            originator: PromptOriginator::User,
        }),
        Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            agent_id: agent_id("agent-1"),
            originator: PromptOriginator::User,
        }),
        Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("hello".to_owned()),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "no live provider".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: Some(ProgressUpdate {
                current: Some(1),
                total: Some(10),
            }),
            display: None,
        }),
        Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: "std-email".into(),
            instance_id: 7.into(),
            schema: action_schema_fixture(),
        }),
        Event::ActionInvoke(ActionInvoke {
            invocation_id: "act-1".into(),
            session_id: "s1".into(),
            extension_name: "std-email".into(),
            instance_id: 7.into(),
            action_id: "email.out.list".to_owned(),
            raw_line: "/email out list".to_owned(),
            argv: Vec::new(),
            arguments: CborValue::Map(Vec::new()),
        }),
        Event::ActionResult(ActionResult {
            invocation_id: "act-1".into(),
            action_id: "email.out.list".to_owned(),
            output: ActionOutput::Text {
                text: "no queued mail".to_owned(),
            },
        }),
        Event::ActionError(ActionError {
            invocation_id: "act-2".into(),
            action_id: "email.out.list".to_owned(),
            message: "approval queue unavailable".to_owned(),
            details: None,
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".to_owned(),
            agent_id: agent_id("agent"),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::AgentMessageSent(AgentMessageSent {
            message_id: "msg-1".into(),
            sender_id: agent_id("engineer_abcd1234"),
            recipient: AgentMessageRecipient::User,
            kind: AgentMessageKind::Message,
            message: "hello".to_owned(),
        }),
        Event::AgentMessageReceived(AgentMessageReceived {
            message_id: "msg-2".into(),
            sender_id: agent_id("engineer_abcd1234"),
            sender_session_id: None,
            recipient_id: agent_id("reviewer_efgh5678"),
            kind: AgentMessageKind::Message,
            message: "hello back".to_owned(),
        }),
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::SessionAgentLoaded(SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id("engineer_abcd1234"),
            ephemeral: false,
        }),
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            agent_id: agent_id("engineer_abcd1234"),
            text: "hello".to_owned(),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentCompactionTriggered(AgentCompactionTriggered {
            agent_id: agent_id("engineer_abcd1234"),
            originator: PromptOriginator::User,
        }),
        Event::AgentPromptCreated(AgentPromptCreated {
            agent_prompt_id: "sp-1".into(),
            agent_id: agent_id("engineer_abcd1234"),
            session_id: "session_123".into(),
            system_prompt: "You are helpful.".to_owned(),
            context: PromptContext {
                blocks: vec![ContextBlock::UserInput(UserInputBlock {
                    items: vec![user_text_item("hello")],
                })],
            },
            tools: vec![ToolDefinition {
                name: ToolName::new("read"),
                model_visible_name: None,
                description: Some("Read a file".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
            }],
            tools_ref: None,
            model: "test/model".parse().expect("model id"),
            model_params: ModelParams::default(),
            tool_choice: ToolChoice::default(),
            originator: PromptOriginator::User,
            ctx_id: None,
            compaction: None,
            share_user_cache_key: false,
        }),
        Event::AgentPromptTerminated(AgentPromptTerminated {
            agent_id: agent_id("engineer_abcd1234"),
            agent_prompt_id: "sp-stale".into(),
            reason: AgentPromptTerminationReason::Stale,
            originator: PromptOriginator::User,
        }),
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-1".into(),
            agent_id: agent_id("engineer_abcd1234"),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "Hi there".to_owned(),
                }],
                phase: None,
            })],
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            usage: None,
            originator: PromptOriginator::User,

            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
        Event::ExtensionStarting(ExtensionStarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionReady(ExtensionReady {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionExited(ExtensionExited {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            exit_code: Some(0),
            signal: None,
        }),
        Event::ExtensionRestarting(ExtensionRestarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            attempt: 2,
            reason: Some("hot reload".to_owned()),
        }),
        Event::ExtSkillAvailable(ExtSkillAvailable {
            name: "brave-search".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        }),
        Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
            file_path: "/home/user/src/project/AGENTS.md".into(),
            content: "# Project instructions\n- Run tests".to_owned(),
        }),
        Event::ExtensionContextReady(ExtensionContextReady {
            session_id: "s1".into(),
            agent_id: agent_id("agent-1"),
        }),
        Event::ExtensionEvent(
            CustomEvent::try_new(
                "demo.progress".parse().expect("event name"),
                Some("s1".into()),
                CborValue::Text("working".to_owned()),
            )
            .expect("valid custom event"),
        ),
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: "openai/gpt-4.1".parse().expect("model id"),
                display_name: Some("GPT-4.1".to_owned()),
                tags: Vec::new(),
                default_affinity: 0,
                context_window: 128_000,
                efforts: vec![Effort::Off, Effort::Low, Effort::Medium, Effort::High],
                verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
            }],
        }),
        Event::ProviderToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("provider-visible completion".to_owned()),
            kind: ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ProviderToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "provider-visible failure".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::HarnessVerbositiesAvailable(HarnessVerbositiesAvailable {
            levels: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
        }),
        Event::HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable {
            levels: vec![
                ThinkingSummary::Off,
                ThinkingSummary::Auto,
                ThinkingSummary::Concise,
                ThinkingSummary::Detailed,
            ],
        }),
        Event::UiRoleUpdate(UiRoleUpdate {
            role: "engineer".to_owned(),
            action: UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
            },
        }),
        Event::UiRoleUpdate(UiRoleUpdate {
            role: "engineer".to_owned(),
            action: UiRoleUpdateAction::SetThinkingSummary {
                thinking_summary: Some(ThinkingSummary::Auto),
            },
        }),
    ]
}

fn sample_session_started() -> Event {
    Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    })
}

fn representative_input_messages() -> Vec<HarnessInputMessage> {
    vec![
        HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "provider".into(),
            client_kind: ClientKind::Provider,
        }),
        HarnessInputMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
        }),
        HarnessInputMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority::new(0),
        }),
        HarnessInputMessage::Ready(Ready {
            message: Some("ready".to_owned()),
        }),
        HarnessInputMessage::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        HarnessInputMessage::ConfigError(ConfigError {
            message: "bad config".to_owned(),
        }),
        HarnessInputMessage::Emit(Emit {
            event: Box::new(Event::ExtensionEvent(
                CustomEvent::try_new(
                    "demo.transient_progress".parse().expect("event name"),
                    Some("s1".into()),
                    CborValue::Text("working".to_owned()),
                )
                .expect("valid custom event"),
            )),
            transient: true,
        }),
        HarnessInputMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        }),
        HarnessInputMessage::GetAgentPromptCreated(GetAgentPromptCreated {
            request_id: "prompt-1".to_owned(),
            session_id: "s1".into(),
            agent_prompt_id: "sp-1".into(),
        }),
        HarnessInputMessage::GetRenderedSystemPrompt(GetRenderedSystemPrompt {
            request_id: "render-system-prompt-1".to_owned(),
            role: "engineer".to_owned(),
        }),
        HarnessInputMessage::GetRenderedPrompt(GetRenderedPrompt {
            request_id: "render-prompt-1".to_owned(),
            role: "engineer".to_owned(),
            enable_agents_md: true,
        }),
        HarnessInputMessage::GetRenderedToolDefinitions(GetRenderedToolDefinitions {
            request_id: "render-tools-1".to_owned(),
            role: "engineer".to_owned(),
        }),
        HarnessInputMessage::ExtensionDataRequest(ExtensionDataRequest {
            request_id: "ext-data-1".to_owned(),
            scope: ExtensionDataScope::Session,
            op: ExtensionDataRequestOp::ReadFile {
                path: ExtensionDataPath::new("notes/state.cbor"),
            },
        }),
        HarnessInputMessage::ExternalAgentMessage(ExternalAgentMessageRequest {
            request_id: "external-1".to_owned(),
            message_id: "msg-external-1".into(),
            sender_session_id: "sender-session".into(),
            sender_id: agent_id("sender_agent"),
            recipient_session_id: "recipient-session".into(),
            recipient_id: agent_id("recipient_agent"),
            kind: AgentMessageKind::Message,
            message: "hello external".to_owned(),
        }),
    ]
}

fn representative_output_messages() -> Vec<HarnessOutputMessage> {
    vec![
        HarnessOutputMessage::Configure(Configure {
            instance_name: None,
            config: CborValue::Null,
            state_dir: Some(std::path::PathBuf::from("/tmp/tau/state/ext/demo")),
            secrets: std::collections::BTreeMap::new(),
        }),
        HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        HarnessOutputMessage::Deliver(EventDelivery::live(
            UnixMicros::new(1_700_000_000_000_000),
            sample_session_started(),
        )),
        HarnessOutputMessage::Deliver(EventDelivery::replay(
            UnixMicros::new(1_700_000_000_000_000),
            sample_session_started(),
        )),
        HarnessOutputMessage::Deliver(EventDelivery::direct(Event::ExtensionEvent(
            CustomEvent::try_new(
                "demo.snapshot".parse().expect("event name"),
                Some("s1".into()),
                CborValue::Text("snapshot".to_owned()),
            )
            .expect("valid custom event"),
        ))),
        HarnessOutputMessage::InterceptRequest(InterceptRequest {
            event: Box::new(sample_session_started()),
            transient: false,
        }),
        HarnessOutputMessage::AgentPromptCreatedResult(Box::new(AgentPromptCreatedResult {
            request_id: "prompt-1".to_owned(),
            prompt: None,
        })),
        HarnessOutputMessage::RenderedSystemPromptResult(Box::new(RenderedSystemPromptResult {
            request_id: "render-system-prompt-1".to_owned(),
            prompt: Some("You are helpful.".to_owned()),
            error: None,
        })),
        HarnessOutputMessage::RenderedPromptResult(Box::new(RenderedPromptResult {
            request_id: "render-prompt-1".to_owned(),
            prompt: Some("<message role=\"system\">\nYou are helpful.\n</message>\n".to_owned()),
            error: None,
        })),
        HarnessOutputMessage::RenderedToolDefinitionsResult(Box::new(
            RenderedToolDefinitionsResult {
                request_id: "render-tools-1".to_owned(),
                tools: Some(vec![ToolDefinition {
                    name: ToolName::new("read"),
                    model_visible_name: None,
                    description: Some("Read a file".to_owned()),
                    tool_type: ToolType::Function,
                    parameters: Some(serde_json::json!({"type": "object"})),
                    format: None,
                }]),
                error: None,
            },
        )),
        HarnessOutputMessage::ExtensionDataResult(Box::new(ExtensionDataResult {
            request_id: "ext-data-1".to_owned(),
            result: ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::ListFiles {
                    entries: vec![ExtensionDataEntry {
                        path: ExtensionDataPath::new("notes/state.cbor"),
                        is_dir: false,
                        len: Some(3),
                    }],
                },
            },
        })),
        HarnessOutputMessage::ExternalAgentMessageResult(ExternalAgentMessageResult {
            request_id: "external-1".to_owned(),
            error: None,
        }),
    ]
}

/// Ensures parsed event names preserve category/call structure and display back
/// to the dotted wire name.
#[test]
fn event_name_round_trips_from_string() {
    for event in representative_events() {
        let name = event.name();
        let serialized = name.to_string();
        assert_eq!(serialized.parse::<EventName>(), Ok(name));
    }
}

/// Ensures parsed event names reject malformed segment structure so custom
/// events cannot enter routing with malformed dotted names.
#[test]
fn event_name_rejects_empty_segments() {
    for name in [".progress", "demo.", ".", "demo.extra.progress"] {
        assert!(name.parse::<EventName>().is_err());
    }

    assert!("demo.progress".parse::<EventName>().is_ok());
}

/// Ensures agent-message event variants report stable event names and
/// persistence defaults.
#[test]
fn agent_message_events_have_names_and_persistence_defaults() {
    let sent = Event::AgentMessageSent(AgentMessageSent {
        message_id: "msg-1".into(),
        sender_id: agent_id("engineer_abcd1234"),
        recipient: AgentMessageRecipient::User,
        kind: AgentMessageKind::Message,
        message: "hello".to_owned(),
    });
    assert_eq!(sent.name(), EventName::AGENT_MESSAGE_SENT);
    assert_eq!(sent.name().to_string(), "agent.message_sent");
    assert!(!sent.defaults_to_transient());

    let received = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: "msg-2".into(),
        sender_id: agent_id("engineer_abcd1234"),
        sender_session_id: None,
        recipient_id: agent_id("reviewer_efgh5678"),
        kind: AgentMessageKind::Message,
        message: "hello back".to_owned(),
    });
    assert_eq!(received.name(), EventName::AGENT_MESSAGE_RECEIVED);
    assert_eq!(received.name().to_string(), "agent.message_received");
    assert!(!received.defaults_to_transient());
}

/// Ensures legacy agent-message payloads omit the default message kind but
/// preserve non-default watch responses.
#[test]
fn agent_message_kind_defaults_and_serializes_only_when_non_default() {
    let legacy: AgentMessageReceived = serde_json::from_value(serde_json::json!({
        "message_id": "msg-legacy",
        "sender_id": "engineer_abcd1234",
        "recipient_id": "reviewer_efgh5678",
        "message": "hello"
    }))
    .expect("legacy message without kind decodes");
    assert_eq!(legacy.kind, AgentMessageKind::Message);

    let explicit_message = AgentMessageReceived {
        message_id: "msg-message".into(),
        sender_id: agent_id("engineer_abcd1234"),
        sender_session_id: None,
        recipient_id: agent_id("reviewer_efgh5678"),
        kind: AgentMessageKind::Message,
        message: "hello".to_owned(),
    };
    let message_json = serde_json::to_value(&explicit_message).expect("serialize message");
    assert_eq!(message_json.get("kind"), None);

    let watch_response = AgentMessageReceived {
        kind: AgentMessageKind::WatchResponse,
        ..explicit_message
    };
    let watch_json = serde_json::to_value(&watch_response).expect("serialize watch response");
    assert_eq!(watch_json["kind"], serde_json::json!("watch_response"));
}

/// Ensures representative harness input/output messages round-trip through the
/// CBOR codec.
#[test]
fn representative_directional_messages_round_trip_through_cbor() {
    for message in representative_input_messages() {
        let encoded = encode_harness_input_to_vec(&message).expect("input should encode");
        let decoded = decode_harness_input_from_slice(&encoded).expect("input should decode");
        assert_eq!(decoded, message);
    }

    for message in representative_output_messages() {
        let encoded = encode_harness_output_to_vec(&message).expect("output should encode");
        let decoded = decode_harness_output_from_slice(&encoded).expect("output should decode");
        assert_eq!(decoded, message);
    }
}

/// Ensures extension-data path wrappers keep the existing string wire shape
/// while giving Rust callers semantic path fields.
#[test]
fn extension_data_paths_use_string_wire_shape() {
    let op = ExtensionDataRequestOp::RenameFile {
        from: ExtensionDataPath::new("old/name"),
        to: ExtensionDataPath::new("new/name"),
    };

    let value = serde_json::to_value(&op).expect("operation should serialize");

    assert_eq!(
        value,
        serde_json::json!({
            "op": "rename_file",
            "from": "old/name",
            "to": "new/name"
        })
    );

    let decoded: ExtensionDataRequestOp =
        serde_json::from_value(value).expect("operation should deserialize");
    assert_eq!(decoded, op);

    let entry = ExtensionDataEntry {
        path: ExtensionDataPath::new("listed/name"),
        is_dir: false,
        len: Some(4),
    };
    let value = serde_json::to_value(&entry).expect("entry should serialize");
    assert_eq!(value["path"], "listed/name");
    let decoded: ExtensionDataEntry =
        serde_json::from_value(value).expect("entry should deserialize");
    assert_eq!(decoded, entry);
}

/// Ensures single-slice decoders reject extra bytes instead of accepting a
/// valid message prefix and ignoring trailing garbage.
#[test]
fn decode_message_from_slice_rejects_trailing_bytes() {
    let message = HarnessInputMessage::Ready(Ready { message: None });
    let mut encoded = encode_harness_input_to_vec(&message).expect("message should encode");
    encoded.extend_from_slice(&[0xff, 0x00]);

    let error = decode_harness_input_from_slice(&encoded).expect_err("trailing bytes should fail");

    assert!(
        error.to_string().contains("trailing bytes"),
        "unexpected error: {error}"
    );
}

/// Ensures framed readers can decode multiple back-to-back protocol messages
/// from one stream.
#[test]
fn multiple_directional_messages_can_share_one_stream() {
    let messages = representative_output_messages();
    let mut writer = HarnessOutputWriter::new(Vec::new());
    for message in &messages {
        writer
            .write_message(message)
            .expect("output message should encode");
    }
    writer.flush().expect("stream should flush");

    let bytes = writer.into_inner();
    let mut reader = HarnessOutputReader::new(std::io::Cursor::new(bytes));
    let mut decoded = Vec::new();
    for _ in 0..messages.len() {
        decoded.push(
            reader
                .read_message()
                .expect("read should succeed")
                .expect("message should arrive"),
        );
    }

    assert_eq!(decoded, messages);
}

/// Ensures extension-defined events cannot spoof first-party event categories
/// that routing and policy code treat as typed protocol events.
#[test]
fn custom_event_rejects_reserved_event_names() {
    let value = serde_json::json!({
        "event": "extension.event",
        "payload": {
            "name": "harness.notice",
            "payload": "spoofed"
        }
    });

    let error = serde_json::from_value::<Event>(value).expect_err("reserved name should fail");

    assert!(
        error.to_string().contains("extension-owned category"),
        "unexpected error: {error}"
    );
}

/// Ensures custom event validation treats manually constructed `Other` values
/// with reserved wire text as reserved categories.
#[test]
fn custom_event_rejects_reserved_category_spelled_as_other() {
    let name = EventName::new(
        EventCategory::Other("harness".to_owned()),
        "info".to_owned(),
    );

    assert!(!CustomEvent::name_is_allowed(&name));

    let error = CustomEvent::try_new(name.clone(), None, CborValue::Null)
        .expect_err("reserved custom event name should fail");
    assert_eq!(error.name(), &name);
    assert_eq!(error.into_name(), name);
}
/// Ensures the panicking EventName constructor enforces dynamic segment
/// validation for public callers.
#[test]
#[should_panic(expected = "invalid event name segment")]
fn event_name_new_panics_on_invalid_segments() {
    let _ = EventName::new(EventCategory::Other("demo.extra".to_owned()), "progress");
}

/// Ensures dynamic event-name construction rejects invalid segment text before
/// custom events can enter routing or serialization.
#[test]
fn custom_event_rejects_direct_empty_segments() {
    assert!(EventName::try_new(EventCategory::Other(String::new()), "progress").is_none());
    assert!(EventName::try_new(EventCategory::Other("demo".to_owned()), String::new()).is_none());
    assert!(
        EventName::try_new(
            EventCategory::Other("harness.notice".to_owned()),
            "progress"
        )
        .is_none()
    );
    assert!(
        EventName::try_new(EventCategory::Other("demo".to_owned()), "extra.progress").is_none()
    );
}

/// Ensures extension-owned custom event categories still round-trip and route
/// by their payload name.
#[test]
fn custom_event_allows_extension_owned_event_names() {
    let event = Event::ExtensionEvent(
        CustomEvent::try_new(
            "demo.progress".parse().expect("custom event name"),
            None,
            CborValue::Text("working".to_owned()),
        )
        .expect("valid custom event"),
    );

    let encoded = serde_json::to_value(&event).expect("serialize custom event");
    let decoded: Event = serde_json::from_value(encoded).expect("decode custom event");

    assert_eq!(decoded.name(), "demo.progress".parse().expect("event name"));
    assert_eq!(decoded, event);
}

/// Ensures peer-to-harness emits and harness-to-peer deliveries keep distinct
/// wire tags.
#[test]
fn input_emit_and_output_deliver_are_distinct_wire_messages() {
    let event = sample_session_started();
    let input = HarnessInputMessage::emit_with_transient(event.clone(), true);
    let output =
        HarnessOutputMessage::deliver_live(UnixMicros::new(1_700_000_000_000_000), event.clone());

    let input_json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(input_json["message"], "emit");
    assert_eq!(input_json["payload"]["event"]["event"], "session.started");
    assert_eq!(input_json["payload"]["transient"], true);

    let output_json = serde_json::to_value(&output).expect("serialize output");
    assert_eq!(output_json["message"], "deliver");
    assert_eq!(output_json["payload"]["event"]["event"], "session.started");
    assert_eq!(
        output_json["payload"]["recorded_at"],
        serde_json::json!(1_700_000_000_000_000_u64)
    );
    // Live deliveries omit the replay marker entirely; only replayed
    // history pays for the extra field on the wire.
    assert!(output_json["payload"].get("replay").is_none());
    assert!(output_json["payload"].get("seq").is_none());
    assert!(output_json["payload"].get("transient").is_none());

    let input_bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert!(decode_harness_output_from_slice(&input_bytes).is_err());

    let output_bytes = encode_harness_output_to_vec(&output).expect("encode output");
    assert!(decode_harness_input_from_slice(&output_bytes).is_err());
}

/// Ensures raw events are not accepted where directional protocol messages are
/// required.
#[test]
fn bare_event_is_not_a_protocol_item_in_either_direction() {
    let bytes = encode_message_to_vec(&sample_session_started()).expect("encode bare event");
    assert!(decode_harness_input_from_slice(&bytes).is_err());
    assert!(decode_harness_output_from_slice(&bytes).is_err());
}

/// Ensures older configure payloads without state_dir still deserialize
/// successfully.
#[test]
fn configure_state_dir_is_optional_for_older_payloads() {
    // Older harnesses sent only `config`. New extensions must still accept that
    // payload and treat the state directory as unavailable rather than failing
    // deserialization during the lifecycle handshake.
    let parsed: Configure = serde_json::from_value(serde_json::json!({
        "config": null
    }))
    .expect("legacy configure decodes");

    assert_eq!(parsed.config, CborValue::Null);
    assert_eq!(parsed.state_dir, None);
    assert!(parsed.secrets.is_empty());

    let with_state = Configure {
        instance_name: None,
        config: CborValue::Null,
        state_dir: Some(std::path::PathBuf::from("/tmp/tau/state/ext/demo")),
        secrets: std::collections::BTreeMap::new(),
    };
    let json = serde_json::to_value(&with_state).expect("serialize configure");
    assert_eq!(
        json["state_dir"],
        serde_json::json!("/tmp/tau/state/ext/demo")
    );
    let decoded: Configure = serde_json::from_value(json).expect("decode configure");
    assert_eq!(decoded, with_state);

    let without_state = serde_json::to_value(Configure {
        instance_name: None,
        config: CborValue::Null,
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
    .expect("serialize configure without state dir");
    assert!(without_state.get("state_dir").is_none());
}

/// Ensures configure secrets round-trip while Debug output redacts secret
/// material.
#[test]
fn configure_secrets_round_trip_and_debug_redacts_values() {
    // Secret values travel only to explicitly configured extensions and must not
    // leak through derived protocol debug output.
    let mut secrets = std::collections::BTreeMap::new();
    secrets.insert("mail_password".to_owned(), SecretValue::new("super-secret"));
    let configure = Configure {
        instance_name: None,
        config: CborValue::Null,
        state_dir: None,
        secrets,
    };

    let debug = format!("{configure:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("super-secret"));

    let json = serde_json::to_value(&configure).expect("serialize configure");
    assert_eq!(
        json["secrets"]["mail_password"],
        serde_json::json!("super-secret")
    );
    let decoded: Configure = serde_json::from_value(json).expect("decode configure");
    assert_eq!(
        decoded.secrets["mail_password"].expose_secret(),
        "super-secret"
    );
}

/// Ensures directional protocol messages use the expected flat tagged wire
/// representation.
#[test]
fn directional_message_wire_form_uses_flat_message_tag() {
    let input = HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "provider".into(),
        client_kind: ClientKind::Provider,
    });
    let input_json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(input_json["message"], "hello");
    assert!(input_json.get("payload").is_some());

    let output = HarnessOutputMessage::Disconnect(Disconnect {
        reason: Some("shutdown".to_owned()),
    });
    let output_json = serde_json::to_value(&output).expect("serialize output");
    assert_eq!(output_json["message"], "disconnect");
    assert!(output_json.get("payload").is_some());
}

/// Ensures events serialize with dotted event names as the wire tag.
#[test]
fn event_wire_form_uses_dotted_event_tag() {
    let event = Event::ToolStarted(ToolStarted {
        call_id: "call-1".into(),
        tool_name: ToolName::new("echo"),
        arguments: CborValue::Text("hi".to_owned()),
        agent_id: agent_id("agent-1"),
        originator: PromptOriginator::User,
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "tool.started");
    assert!(json.get("payload").is_some());
}

/// Extension prompt submit requests are a request/intake event, not a durable
/// transcript fact. This locks in the explicit `extension.*` wire name.
#[test]
fn extension_prompt_submit_request_wire_form() {
    let event = Event::ExtPromptSubmitRequest(ExtPromptSubmitRequest {
        agent_id: agent_id("agent-1"),
        text: "[telegram from alice] hello".to_owned(),
        ctx_id: Some("ctx-1".to_owned()),
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "extension.prompt_submit_request");
    assert_eq!(json["payload"]["agent_id"], "agent-1");
    assert_eq!(json["payload"]["ctx_id"], "ctx-1");
    assert_eq!(event.name(), EventName::EXTENSION_PROMPT_SUBMIT_REQUEST);
}

/// Ensures model ids split only on the first slash so provider model names may
/// contain slashes.
#[test]
fn model_id_parses_provider_and_slashy_model_name() {
    // OpenRouter and similar providers use native model ids such as
    // `anthropic/claude-sonnet-4`. The first slash separates Tau's provider
    // namespace; remaining slashes belong to the provider-native model id.
    let model: ModelId = "openrouter/anthropic/claude-sonnet-4"
        .parse()
        .expect("model id");

    assert_eq!(model.provider.as_str(), "openrouter");
    assert_eq!(model.model.as_str(), "anthropic/claude-sonnet-4");
    assert_eq!(model.to_string(), "openrouter/anthropic/claude-sonnet-4");
}

/// Ensures provider model-list events use the provider event namespace.
#[test]
fn provider_models_updated_name_matches_wire_family() {
    // `provider.models_updated` is routed by event name, so `Event::name()` must
    // match the serde tag exactly. A past implementation accidentally reported
    // this as `extension.provider_models_updated`, which made prefix selectors
    // and debug output disagree with the wire protocol.
    let event = Event::ProviderModelsUpdated(ProviderModelsUpdated { models: Vec::new() });

    assert_eq!(event.name().to_string(), "provider.models_updated");
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "provider.models_updated");
}

/// Ensures execution lifecycle events retain the provider wire-family event
/// names.
#[test]
fn execution_events_use_provider_wire_family() {
    // Provider extensions own execution status; agent transcript events use
    // `agent.*`, but provider execution progress remains in the `provider.*`
    // family so subscribers can route it separately.
    let cases = [
        (
            Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
                agent_prompt_id: "sp-1".into(),
                originator: PromptOriginator::User,
            }),
            "provider.prompt_submitted",
        ),
        (
            Event::ProviderResponseUpdated(ProviderResponseUpdated {
                agent_prompt_id: "sp-1".into(),
                agent_id: agent_id("engineer_abcd1234"),
                deltas: Vec::new(),
                compaction: None,
                status: None,
                originator: PromptOriginator::User,
            }),
            "provider.response_updated",
        ),
        (
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: "sp-1".into(),
                agent_id: agent_id("engineer_abcd1234"),
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
            }),
            "provider.response_finished",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["event"], expected);
    }
}

/// Ensures provider response updates require the new delta-routing fields
/// rather than accepting legacy text/thinking snapshots.
#[test]
fn provider_response_updated_requires_delta_routing_fields() {
    // The provider streaming payload is delta-based. Legacy text/thinking
    // snapshots must fail instead of silently decoding as empty delta updates.
    let value = serde_json::json!({
        "agent_prompt_id": "sp-1",
        "agent_id": "engineer_abcd1234",
        "text": "legacy assistant text",
        "thinking": "legacy reasoning text"
    });

    let error = serde_json::from_value::<ProviderResponseUpdated>(value)
        .expect_err("legacy streaming payload should not decode");
    assert!(
        error.to_string().contains("text"),
        "unexpected error: {error}"
    );
}

/// Ensures the provider repetition stop reason has a stable protocol spelling.
#[test]
fn provider_stop_reason_repetition_detected_uses_snake_case_wire_value() {
    let json = serde_json::to_value(ProviderStopReason::RepetitionDetected)
        .expect("serialize stop reason");
    assert_eq!(json, serde_json::json!("repetition_detected"));
    let decoded: ProviderStopReason =
        serde_json::from_value(json).expect("deserialize stop reason");
    assert_eq!(decoded, ProviderStopReason::RepetitionDetected);
}

/// Ensures harness role info remains backward compatible when role descriptions
/// are omitted.
#[test]
fn harness_role_info_role_description_is_optional_and_round_trips() {
    // Older harnesses only send `description`; the new free-form role metadata
    // must default cleanly while preserving the technical description field.
    let legacy: HarnessRoleInfo = serde_json::from_value(serde_json::json!({
        "name": "engineer",
        "description": "model=openai/gpt-4.1, effort=high"
    }))
    .expect("decode legacy role info");
    assert_eq!(legacy.name, "engineer");
    assert_eq!(legacy.role_description, None);

    let with_description = HarnessRoleInfo {
        name: "deep".to_owned(),
        description: "model=openai/gpt-4.1, effort=xhigh".to_owned(),
        role_description: Some("Deep investigation mode".to_owned()),
        details: Some(HarnessRoleDetails {
            model: Some("openai/gpt-4.1".into()),
            params: ModelParams {
                effort: Effort::High,
                verbosity: Verbosity::Medium,
                thinking_summary: ThinkingSummary::Auto,
                service_tier: Some(ServiceTier::Fast),
            },
            tools: Some(vec![ToolName::new("read")]),
            enable_tool_groups: vec![ToolGroupName::new("pim")],
            disable_tool_groups: vec![ToolGroupName::new("shell")],
            enable_tools: vec![ToolName::new("web_search")],
            disable_tools: vec![ToolName::new("shell")],
        }),
    };
    let json = serde_json::to_value(&with_description).expect("serialize role info");
    assert_eq!(json["role_description"], "Deep investigation mode");
    assert_eq!(json["details"]["model"], "openai/gpt-4.1");
    assert_eq!(json["details"]["params"]["effort"], "high");
    assert_eq!(json["details"]["enable_tools"][0], "web_search");
    let decoded: HarnessRoleInfo = serde_json::from_value(json).expect("decode role info");
    assert_eq!(decoded, with_description);

    let without_description = serde_json::to_value(HarnessRoleInfo {
        role_description: None,
        ..with_description
    })
    .expect("serialize role info without metadata");
    assert!(without_description.get("role_description").is_none());
}

/// Ensures provider model metadata rejects missing context-window limits
/// required by scheduling/UI code.
#[test]
fn provider_model_info_requires_context_window() {
    // The harness uses provider snapshots as the only source of model UI
    // metadata, so context windows must be present instead of defaulted.
    let value = serde_json::json!({
        "id": "openai/gpt-4.1",
        "efforts": ["off"],
        "verbosities": ["medium"],
        "thinking_summaries": ["off"]
    });

    let error = serde_json::from_value::<ProviderModelInfo>(value)
        .expect_err("context_window should be required");
    assert!(
        error.to_string().contains("context_window"),
        "unexpected error: {error}"
    );
}

/// Ensures JSON-to-CBOR conversion preserves unsigned integer precision above
/// the exact range of IEEE-754 floats.
#[test]
fn json_to_cbor_preserves_large_unsigned_integers() {
    let max = serde_json::json!(u64::MAX);
    assert_eq!(
        json_to_cbor(&max),
        CborValue::Integer(ciborium::value::Integer::from(u64::MAX))
    );

    let above_precise_float = serde_json::json!(9_007_199_254_740_993_u64);
    assert_eq!(
        json_to_cbor(&above_precise_float),
        CborValue::Integer(ciborium::value::Integer::from(9_007_199_254_740_993_u64))
    );
}

/// Ensures valid tool identifiers are accepted by the ToolName validator.
#[test]
fn tool_name_accepts_valid_names() {
    assert!(ToolName::try_new("read").is_some());
    assert!(ToolName::try_new("shell").is_some());
    assert!(ToolName::try_new("my_tool_2").is_some());
    assert!(ToolName::try_new("Echo").is_some());
}

/// Ensures ToolName rejects empty names and names with unsupported separators
/// or whitespace.
#[test]
fn tool_name_rejects_invalid_names() {
    assert!(ToolName::try_new("").is_none());
    assert!(ToolName::try_new("fs.read").is_none());
    assert!(ToolName::try_new("my tool").is_none());
    assert!(ToolName::try_new("a-b").is_none());
    assert!(ToolName::try_new("tool/name").is_none());
}

/// Ensures the panicking ToolName constructor fails fast on invalid
/// identifiers.
#[test]
#[should_panic(expected = "invalid tool name")]
fn tool_name_new_panics_on_invalid() {
    let _ = ToolName::new("bad.name");
}

/// Ensures ToolName enforces its maximum byte length.
#[test]
fn tool_name_rejects_overlong_input() {
    // ASCII alphanumerics that exceed the cap must be rejected even
    // though they pass the character-class check.
    let long = "a".repeat(ToolName::MAX_LEN + 1);
    assert!(ToolName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolName::MAX_LEN);
    assert!(ToolName::try_new(at_cap).is_some());
}

/// Ensures model/tool policy tags accept lowercase namespaced values and reject
/// nondeterministic spellings such as uppercase or whitespace.
#[test]
fn model_and_tool_tags_validate_namespaced_lowercase_values() {
    assert!(ModelTag::try_new("shell:chatgpt").is_some());
    assert!(ToolTag::try_new("shell:edit:apply_patch").is_some());
    assert!(ToolTag::try_new("tools:custom-text").is_some());
    assert!(ModelTag::try_new("Shell:ChatGPT").is_none());
    assert!(ToolTag::try_new("shell edit").is_none());
    assert!(ToolTag::try_new("a".repeat(ToolTag::MAX_LEN + 1)).is_none());
}

/// Ensures tool group names enforce the same valid identifier shape as tool
/// names.
#[test]
fn tool_group_name_accepts_valid_names() {
    assert!(ToolGroupName::try_new("mail").is_some());
    assert!(ToolGroupName::try_new("project_tools_2").is_some());
    assert!(ToolGroupName::try_new("Ops").is_some());
}

/// Ensures invalid tool group names cannot enter the protocol through fallible
/// construction.
#[test]
fn tool_group_name_rejects_invalid_names() {
    assert!(ToolGroupName::try_new("").is_none());
    assert!(ToolGroupName::try_new("mail.send").is_none());
    assert!(ToolGroupName::try_new("mail send").is_none());
    assert!(ToolGroupName::try_new("mail-send").is_none());
    assert!(ToolGroupName::try_new("mail/send").is_none());
}

/// Ensures overlong tool group names are rejected before they can be
/// serialized.
#[test]
fn tool_group_name_rejects_overlong_input() {
    let long = "a".repeat(ToolGroupName::MAX_LEN + 1);
    assert!(ToolGroupName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolGroupName::MAX_LEN);
    assert!(ToolGroupName::try_new(at_cap).is_some());
}

/// Ensures event-delivery helpers preserve replay/live state and expose the
/// wrapped event.
#[test]
fn event_delivery_helpers_expose_replay_marker_and_inner_event() {
    // The replay marker is the contract side-effecting consumers rely on to
    // skip historical frames; live and direct deliveries must not carry it.
    let inner = sample_session_started();
    let message =
        HarnessOutputMessage::deliver_live(UnixMicros::new(1_700_000_000_000_000), inner.clone());

    let delivery = message.as_delivery().expect("delivery payload");
    assert!(!delivery.is_replay());
    assert_eq!(delivery.event(), &inner);
    assert_eq!(message.clone().into_delivered_event(), Some(inner.clone()));

    let replayed = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1_700_000_000_000_000),
        sample_session_started(),
    );
    assert!(replayed.as_delivery().expect("delivery").is_replay());

    let direct = HarnessOutputMessage::deliver(sample_session_started());
    assert!(!direct.as_delivery().expect("delivery").is_replay());

    let non_delivery = HarnessOutputMessage::Disconnect(Disconnect { reason: None });
    assert_eq!(non_delivery.as_delivery(), None);
    assert_eq!(non_delivery.into_delivered_event(), None);
}

/// Ensures transient-default classification matches progress-style events.
#[test]
fn event_defaults_to_transient_marks_progress_kinds() {
    // The set named by `defaults_to_transient` is the contract the
    // harness relies on to decide which events skip durable semantic
    // logs when a component publishes them without explicit transient
    // metadata. Lock it down here so any future
    // edit to the matcher is intentional.
    let transient = [
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: "sp-1".into(),
            agent_id: agent_id("engineer_abcd1234"),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: None,
            display: None,
        }),
        Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: "std-email".into(),
            instance_id: 7.into(),
            schema: action_schema_fixture(),
        }),
        Event::ActionInvoke(ActionInvoke {
            invocation_id: "act-1".into(),
            session_id: "s1".into(),
            extension_name: "std-email".into(),
            instance_id: 7.into(),
            action_id: "email.out.list".to_owned(),
            raw_line: "/email out list".to_owned(),
            argv: Vec::new(),
            arguments: CborValue::Map(Vec::new()),
        }),
        Event::ActionResult(ActionResult {
            invocation_id: "act-1".into(),
            action_id: "email.out.list".to_owned(),
            output: ActionOutput::Text {
                text: "ok".to_owned(),
            },
        }),
        Event::ActionError(ActionError {
            invocation_id: "act-2".into(),
            action_id: "email.out.list".to_owned(),
            message: "nope".to_owned(),
            details: None,
        }),
        Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1".into(),
            text: "draft".to_owned(),
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".to_owned(),
            agent_id: agent_id("agent"),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: agent_id("worker"),
            text: "queued".to_owned(),
            message_class: PromptMessageClass::User,
        }),
        Event::AgentPromptRecalled(AgentPromptRecalled {
            agent_id: agent_id("worker"),
            text: "queued".to_owned(),
        }),
        Event::AgentPromptTerminated(AgentPromptTerminated {
            agent_id: agent_id("worker"),
            agent_prompt_id: "sp-stale".into(),
            reason: AgentPromptTerminationReason::Stale,
            originator: PromptOriginator::User,
        }),
    ];
    for event in &transient {
        assert!(
            event.defaults_to_transient(),
            "{} should default to transient",
            event.name()
        );
    }

    let durable = [
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            agent_id: agent_id("worker"),
            text: "hi".to_owned(),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            display_name: None,
            ctx_id: None,
        }),
        Event::SessionAgentLoaded(SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id("worker"),
            ephemeral: false,
        }),
        Event::AgentMetadataSet(AgentMetadataSet {
            agent_id: agent_id("worker"),
            key: AgentMetadataKey::new("ext_core-shell_cwd"),
            value: CborValue::Text("/tmp".to_owned()),
            inheritable: true,
        }),
        Event::AgentMetadataUnset(AgentMetadataUnset {
            agent_id: agent_id("worker"),
            key: AgentMetadataKey::new("ext_core-shell_cwd"),
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("read"),
            tool_type: ToolType::Function,
            message: "failed".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
    ];
    for event in &durable {
        assert!(
            !event.defaults_to_transient(),
            "{} should be durable",
            event.name()
        );
    }
}

/// Ensures legacy tool-result events without an explicit kind deserialize as
/// final results.
#[test]
fn tool_result_kind_defaults_to_final_for_legacy_events() {
    let result: ToolResult = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "tool_name": "read",
        "tool_type": "function",
        "result": "ok",
        "originator": { "kind": "user" }
    }))
    .expect("legacy tool result decodes");
    assert_eq!(result.kind, ToolResultKind::Final);
}

/// Ensures prompt messages remain backward compatible by defaulting omitted
/// class to user.
#[test]
fn prompt_message_class_defaults_to_user_when_omitted() {
    let prompt: UiPromptSubmitted = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "text": "legacy",
        "agent_id": "agent",
        "originator": { "kind": "user" }
    }))
    .expect("ui prompt decodes");
    assert_eq!(prompt.message_class, PromptMessageClass::User);
    assert!(!prompt.message_class.is_internal());

    let submitted: AgentPromptSubmitted = serde_json::from_value(serde_json::json!({
        "agent_id": "worker",
        "text": "submitted"
    }))
    .expect("agent prompt decodes");
    assert_eq!(submitted.message_class, PromptMessageClass::User);
    assert_eq!(submitted.originator, PromptOriginator::User);

    let queued: AgentPromptQueued = serde_json::from_value(serde_json::json!({
        "agent_id": "worker",
        "text": "queued"
    }))
    .expect("queued prompt decodes");
    assert_eq!(queued.message_class, PromptMessageClass::User);

    let internal = serde_json::to_value(AgentPromptSteered {
        agent_id: agent_id("worker"),
        text: "[tau-internal] Tool call `bg` is complete.".into(),
        message_class: PromptMessageClass::Internal,
    })
    .expect("serialize steered prompt");
    assert_eq!(internal["message_class"], serde_json::json!("internal"));
}

/// Ephemeral-agent markers must be backwards-compatible with peers that do not
/// yet send the field, and compact on the wire for the durable default.
#[test]
fn ephemeral_agent_fields_default_false_and_skip_serializing() {
    let create: UiCreateAgent = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "role": "senior-engineer"
    }))
    .expect("legacy create-agent");
    assert!(!create.ephemeral);

    let started = AgentStarted {
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        parent_agent: None,
        role: "senior-engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    };
    let json = serde_json::to_value(&started).expect("serialize started");
    assert!(json.get("ephemeral").is_none());

    let loaded = SessionAgentLoaded {
        session_id: "s1".into(),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        ephemeral: true,
    };
    let json = serde_json::to_value(&loaded).expect("serialize loaded");
    assert_eq!(json["ephemeral"], serde_json::json!(true));
}

/// Tool specs default to enabled and omit default-valued fields for compact
/// extension registration payloads.
#[test]
fn tool_spec_defaults_and_background_support() {
    let parsed: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "echo",
        "description": "Echo a payload",
        "tool_type": "function"
    }))
    .expect("deserialize tool spec");
    assert!(parsed.enabled_by_default);

    let serialized = serde_json::to_value(&parsed).expect("serialize tool spec");
    assert!(serialized.get("enabled_by_default").is_none());
    assert!(serialized.get("background_support").is_none());
    assert_eq!(parsed.background_support, None);

    let backgrounded: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "agent_start",
        "tool_type": "function",
        "background_support": "instant"
    }))
    .expect("deserialize background support");
    assert_eq!(
        backgrounded.background_support,
        Some(BackgroundSupport::Instant)
    );

    let disabled = ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    };
    let serialized = serde_json::to_value(&disabled).expect("serialize disabled tool spec");
    assert_eq!(
        serialized["enabled_by_default"],
        serde_json::Value::Bool(false)
    );
}

/// Prompt fragment primitives are transparent on the wire so config and
/// extension JSON can stay simple: priorities are numbers and prompt contents
/// are plain strings.
#[test]
fn prompt_fragment_primitives_serde_as_simple_values() {
    let priority: PromptPriority =
        serde_json::from_value(serde_json::json!(42)).expect("deserialize prompt priority");
    let content: PromptContent =
        serde_json::from_value(serde_json::json!("Use care")).expect("deserialize prompt content");

    assert_eq!(priority.get(), 42);
    assert_eq!(content.as_str(), "Use care");
    assert_eq!(
        serde_json::to_value(priority).expect("serialize prompt priority"),
        serde_json::json!(42)
    );
    assert_eq!(
        serde_json::to_value(content).expect("serialize prompt content"),
        serde_json::json!("Use care")
    );
}

fn echo_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

/// `tool.register` remains compatible with extensions that omit prompt
/// fragments, while newer extensions can attach one ordered prompt fragment.
#[test]
fn tool_register_prompt_is_optional_and_round_trips_when_present() {
    let without_prompt: ToolRegister = serde_json::from_value(serde_json::json!({
        "tool": {
            "name": "echo",
            "description": "Echo a payload",
            "tool_type": "function"
        }
    }))
    .expect("deserialize tool register without prompt");
    assert_eq!(without_prompt.prompt_fragment, None);

    let with_prompt = ToolRegister {
        tool: echo_tool_spec(),
        tool_group: None,
        prompt_fragment: Some(PromptFragment::new(
            "echo.instructions",
            PromptPriority::new(7),
            "Prefer the echo tool for echo requests.",
        )),
    };
    let json = serde_json::to_value(&with_prompt).expect("serialize tool register with prompt");
    assert_eq!(json["prompt_fragment"]["priority"], serde_json::json!(7));
    assert_eq!(
        json["prompt_fragment"]["template"],
        serde_json::json!("Prefer the echo tool for echo requests.")
    );
    let decoded: ToolRegister = serde_json::from_value(json).expect("decode prompt fragment");
    assert_eq!(decoded, with_prompt);
}

/// `StartAgentRequest` leaves role selection to the harness when omitted.
#[test]
fn start_agent_request_role_is_optional() {
    let parsed: StartAgentRequest = serde_json::from_value(serde_json::json!({
        "query_id": "q1",
        "instruction": "summarize"
    }))
    .expect("deserialize start-agent request");
    assert_eq!(parsed.role, None);
}

/// `DelegateProgress` UI metadata is additive. Omitting role must stay
/// readable.
#[test]
fn delegate_progress_role_is_optional() {
    let parsed: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress without role");
    assert_eq!(parsed.role, None);

    let with_metadata: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "role": "rush",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress with role");
    assert_eq!(with_metadata.role.as_deref(), Some("rush"));
}

/// `Verbosity::next_in` mirrors `Effort::next_in`. Even though the CLI
/// doesn't bind a cycle key for verbosity today, the helper is part of
/// the public API and the protocol tests should pin the same wrap /
/// skip / empty-allowed-set behaviour effort relies on.
#[test]
fn verbosity_next_in_skips_disallowed_levels_and_wraps() {
    use Verbosity::*;
    let canonical = [Low, Medium, High];

    assert_eq!(Low.next_in(&canonical), Medium);
    assert_eq!(High.next_in(&canonical), Low);

    let only_low_high = [Low, High];
    assert_eq!(Low.next_in(&only_low_high), High);
    assert_eq!(High.next_in(&only_low_high), Low);

    let pinned = [Medium];
    assert_eq!(Low.next_in(&pinned), Medium);
    assert_eq!(Medium.next_in(&pinned), Medium);

    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// `ThinkingSummary` parses from / displays through the canonical
/// wire forms used by slash commands and harness role config.
#[test]
fn thinking_summary_round_trips_through_display_and_from_str() {
    use ThinkingSummary::*;
    for level in [Off, Auto, Concise, Detailed] {
        let s = level.to_string();
        assert_eq!(s.parse::<ThinkingSummary>().ok(), Some(level));
    }
    assert!("bogus".parse::<ThinkingSummary>().is_err());
}

/// `ModelParams` serializes its bundled knobs as a flat object that
/// drops fields at their default value. Lets `harness.yaml`
/// snapshots stay tiny and avoids surprising callers that introspect
/// the wire shape.
#[test]
fn model_params_serializes_skipping_defaults() {
    let json = serde_json::to_value(ModelParams::default()).expect("serialize");
    assert_eq!(json, serde_json::json!({}));

    let json = serde_json::to_value(ModelParams {
        effort: Effort::High,
        verbosity: Verbosity::Low,
        thinking_summary: ThinkingSummary::Concise,
        service_tier: Some(ServiceTier::Fast),
    })
    .expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "effort": "high",
            "thinking_summary": "concise",
            "service_tier": "fast",
        })
    );
}

/// `Effort::next_in` must skip levels that aren't in the harness's
/// allowed set so cycling callers don't trap when (say) `xhigh` is
/// missing for the current model. Locking the behaviour with explicit
/// cases so a future refactor of the cycle helper can't silently
/// regress the UX.
#[test]
fn effort_next_in_skips_disallowed_levels_and_wraps() {
    use Effort::*;
    let canonical = [Off, Minimal, Low, Medium, High];
    let with_xhigh = [Off, Minimal, Low, Medium, High, XHigh];

    // Without xhigh, High wraps back to Off — XHigh is skipped.
    assert_eq!(High.next_in(&canonical), Off);
    // With xhigh, High advances to XHigh and XHigh wraps to Off.
    assert_eq!(High.next_in(&with_xhigh), XHigh);
    assert_eq!(XHigh.next_in(&with_xhigh), Off);

    // Sparse allowed set (provider with no reasoning effort) — Off
    // is the only legal level, so any input lands there.
    let only_off = [Off];
    assert_eq!(High.next_in(&only_off), Off);
    assert_eq!(Off.next_in(&only_off), Off);

    // Empty allowed set falls through to plain `next()` so callers
    // that haven't received `HarnessEffortsAvailable` yet still make
    // progress.
    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// Provider-facing tool responses must use the uniform header/body shape so
/// individual providers do not each invent their own CBOR rendering.
#[test]
fn tool_response_renders_headers_blank_line_and_body() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("/tmp/file".to_owned()),
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("line-numbered content".to_owned()),
            CborValue::Text("1 hello\n2 world".to_owned()),
        ),
    ]));

    assert_eq!(
        response.render(),
        "path: /tmp/file\ntotal_lines: 2\n\n1 hello\n2 world"
    );
}

/// Ensures ToolResponse renders conventional output fields as body text without
/// an extra label.
#[test]
fn tool_response_renders_output_field_as_body_without_label() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            CborValue::Integer(0.into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text("out stdout\nerr stderr".to_owned()),
        ),
    ]));

    assert_eq!(response.render(), "status: 0\n\nout stdout\nerr stderr");
}

/// Ensures ToolResponse rendering does not expose raw CBOR debug structure for
/// output fields.
#[test]
fn tool_response_output_field_hides_raw_data_from_rendered_body() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("format".to_owned()),
            CborValue::Text("name flags".to_owned()),
        ),
        (
            CborValue::Text("data".to_owned()),
            CborValue::Map(vec![
                (
                    CborValue::Text("format".to_owned()),
                    CborValue::Text("name flags".to_owned()),
                ),
                (
                    CborValue::Text("folders".to_owned()),
                    CborValue::Array(vec![CborValue::Text("INBOX selectable".to_owned())]),
                ),
            ]),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text("INBOX selectable".to_owned()),
        ),
    ]));

    assert_eq!(response.render(), "format: name flags\n\nINBOX selectable");
}

/// Ensures plain text tool responses render as body content without synthetic
/// headers.
#[test]
fn tool_response_leaves_plain_text_as_body_only() {
    let response = ToolResponse::from_cbor(&CborValue::Text("done".to_owned()));

    assert_eq!(response.render(), "done");
}

/// Ensures rendered array/map records remain visually separated for provider
/// readability.
#[test]
fn tool_response_separates_array_map_records_with_blank_lines() {
    let response = ToolResponse::from_cbor(&CborValue::Array(vec![
        CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("first".to_owned()),
        )]),
        CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("second".to_owned()),
        )]),
    ]));

    assert_eq!(response.render(), "name: first\n\nname: second");
}

/// Ensures arrays of scalar tool output values render compactly instead of as
/// noisy records.
#[test]
fn tool_response_keeps_scalar_arrays_compact() {
    let response = ToolResponse::from_cbor(&CborValue::Array(vec![
        CborValue::Text("name".to_owned()),
        CborValue::Text("description".to_owned()),
    ]));

    assert_eq!(response.render(), "name\ndescription");
}

/// Provider-visible rendering must not let header keys or values inject extra
/// records or raw terminal controls into model input.
#[test]
fn tool_response_escapes_header_controls() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("bad\nkey".to_owned()),
        CborValue::Text("value\r\u{1b}\0\t\u{85}".to_owned()),
    )]));

    assert_eq!(
        response.render(),
        "bad\\nkey: value\\r\\x1b\\0\\t\\u{85}\n\n"
    );
}

/// Directly constructed ToolResponse headers still pass through render-time
/// sanitization so header values cannot inject lines or raw DEL controls.
#[test]
fn tool_response_escapes_direct_header_value_controls() {
    let response = ToolResponse {
        raw: CborValue::Null,
        headers: vec![ToolResponseHeader {
            key: "status".to_owned(),
            value: "ok\nforged: yes\u{7f}".to_owned(),
        }],
        body: String::new(),
    };

    assert_eq!(response.render(), "status: ok\\nforged: yes\\u{7f}\n\n");
}

/// Body sanitization is last-resort provider safety: it preserves legitimate
/// line-feed record separators but escapes other raw controls.
#[test]
fn tool_response_preserves_body_lfs_but_escapes_controls() {
    let response = ToolResponse::from_cbor(&CborValue::Text(
        "line 1\nline 2\r\u{1b}\0\t\u{85}".to_owned(),
    ));

    assert_eq!(response.render(), "line 1\nline 2\\r\\x1b\\0\\t\\u{85}");
}

/// Unicode line and paragraph separators are not ASCII LF record separators, so
/// they must be escaped in both headers and bodies before model-visible output.
#[test]
fn tool_response_escapes_unicode_line_separators() {
    let header_response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("key\u{2028}next".to_owned()),
        CborValue::Text("value\u{2029}next".to_owned()),
    )]));
    let body_response = ToolResponse::from_cbor(&CborValue::Text(
        "line\u{2028}not-record\u{2029}end".to_owned(),
    ));

    assert_eq!(
        header_response.render(),
        "key\\u{2028}next: value\\u{2029}next\n\n"
    );
    assert_eq!(
        body_response.render(),
        "line\\u{2028}not-record\\u{2029}end"
    );
}

/// Metadata labels that are pushed into a multiline body still need single-line
/// escaping so a malicious key cannot forge additional labels.
#[test]
fn tool_response_escapes_multiline_body_labels() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("label\nforged".to_owned()),
        CborValue::Text("first\nsecond".to_owned()),
    )]));

    assert_eq!(response.render(), "label\\nforged:\nfirst\nsecond");
}

/// Binary fallback rendering stays bounded and does not leak raw bytes into the
/// provider-visible transcript.
#[test]
fn tool_response_renders_bytes_as_bounded_placeholder() {
    let response = ToolResponse::from_cbor(&CborValue::Bytes(vec![0; 1024]));

    assert_eq!(response.render(), "<1024 bytes>");
}

/// Ensures a clear close name is suggested, while poor and tied matches are
/// suppressed so callers do not present misleading recovery hints.
#[test]
fn nearest_name_suggestion_is_conservative_and_tie_safe() {
    assert_eq!(
        nearest_name_suggestion("rea", ["read", "edit"].into_iter()),
        Some("read".to_owned())
    );
    assert_eq!(
        nearest_name_suggestion("xyz", ["read", "edit"].into_iter()),
        None
    );
    assert_eq!(
        nearest_name_suggestion("cat", ["bat", "cut"].into_iter()),
        None
    );
}

/// Ensures suggestion work is bounded by observed candidate count, not only by
/// accepted short candidates after filtering.
#[test]
fn nearest_name_suggestion_suppresses_oversized_candidate_sets() {
    let candidates = (0..=MAX_SUGGESTION_CANDIDATES)
        .map(|idx| format!("{}-{idx}", "x".repeat(MAX_SUGGESTION_NAME_CHARS + 10)))
        .collect::<Vec<_>>();
    let suggestion = nearest_name_suggestion("read", candidates.iter().map(String::as_str));

    assert_eq!(suggestion, None);
}

/// Ensures overlong requested names and candidates are handled without scanning
/// or scoring unbounded strings.
#[test]
fn nearest_name_suggestion_suppresses_overlong_names() {
    let overlong = "r".repeat(MAX_SUGGESTION_NAME_CHARS + 1);
    assert_eq!(
        nearest_name_suggestion(&overlong, ["read"].into_iter()),
        None
    );
    assert_eq!(
        nearest_name_suggestion("reed", ["read", overlong.as_str()].into_iter()),
        Some("read".to_owned())
    );
}

/// Ensures tool examples are protocol-optional so older tool registrations
/// deserialize without changing shape.
#[test]
fn tool_spec_examples_default_to_empty() {
    let value = serde_json::json!({
        "name": "read",
        "tool_type": "function"
    });

    let spec: ToolSpec = serde_json::from_value(value).expect("legacy tool spec");

    assert!(spec.examples.is_empty());
}

/// Ensures example metadata can round-trip when providers opt into it.
#[test]
fn tool_spec_examples_round_trip() {
    let spec = ToolSpec {
        name: ToolName::new("edit"),
        model_visible_name: None,
        description: None,
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "replace".to_owned(),
            title: Some("Replace".to_owned()),
            arguments: CborValue::Map(vec![(
                CborValue::Text("operation".to_owned()),
                CborValue::Text("replace".to_owned()),
            )]),
            note: Some("Use exact field names.".to_owned()),
            subcommand: Some(ToolExampleSelector {
                path: vec!["operation".to_owned()],
                value: CborValue::Text("replace".to_owned()),
            }),
        }],
    };

    let encoded = serde_json::to_value(&spec).expect("serialize");
    let decoded: ToolSpec = serde_json::from_value(encoded).expect("deserialize");

    assert_eq!(decoded, spec);
}
