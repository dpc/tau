use super::*;
use crate::AgentId;
use crate::agent::{Agent, AgentTurnState, PendingPrompt};
use crate::harness::{
    BackgroundCompletionPromptMode, PendingTool, background_completion_prompt,
    extension_disconnected_background_tool_call_error_message,
    extension_disconnected_tool_call_error_message, is_restore_notice_prompt_text,
    restore_notice_prompt_for_elapsed, unavailable_tool_error_message,
};

fn responses_backend() -> tau_proto::ProviderBackend {
    tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::Responses,
        base_url: "https://api.example.test".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

fn publish_pending_agent_context_ready(h: &mut Harness, agent_id: &str) {
    let agent_id = tau_proto::AgentId::parse(agent_id).expect("agent id");
    let Some(source_id) = h
        .pending_agent_context_ready
        .get(&agent_id)
        .and_then(|waiting_on| waiting_on.iter().next())
        .cloned()
    else {
        return;
    };
    h.handle_extension_event(
        source_id.as_str(),
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: h.current_session_id.clone(),
                agent_id,
            },
        )),
    )
    .expect("context ready");
}

/// Regression: startup has no implicit `main` agent. The first interactive
/// prompt claims the default conversation by minting a durable role-prefixed
/// hex agent id, publishes that id on `AgentPromptStarted`/`AgentPromptCreated`
/// for UI/provider routing, and includes the same id in the actual
/// provider-bound default system prompt so the agent knows its own identity
/// without relying only on event metadata. It also locks in that the
/// lightweight lifecycle event immediately precedes the full provider prompt.
#[test]
fn user_prompt_mints_first_agent_for_empty_startup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let existing_agent_id = h
        .agents
        .values()
        .find(|conversation| conversation.originator.is_user())
        .and_then(|conversation| conversation.agent_id.clone());

    h.submit_user_prompt("s1".into(), "hello".to_owned())
        .expect("submit first user prompt");

    let agent_id = h
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.agent_id.as_deref())
        .expect("first prompt minted agent id");
    if let Some(existing_agent_id) = existing_agent_id {
        assert_eq!(agent_id, existing_agent_id);
    } else {
        assert_role_hex_agent_id(agent_id, "engineer");
    }
    assert_eq!(h.agent_routes.get(agent_id), Some(&test_user_agent(&h)));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.agent_id.as_str() == agent_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id.as_str() == agent_id
    )));
    let prompt = read_nth_prompt_created(&h, 0);
    let events = event_log_events(&h);
    let prompt_pair = events
        .windows(2)
        .find_map(|window| match (&window[0], &window[1]) {
            (Event::AgentPromptStarted(started), Event::AgentPromptCreated(created))
                if started.agent_prompt_id == created.agent_prompt_id =>
            {
                Some((started, created))
            }
            _ => None,
        })
        .expect("prompt_started immediately precedes matching prompt_created");
    assert_eq!(prompt_pair.0.agent_id, prompt_pair.1.agent_id);
    assert_eq!(prompt_pair.0.session_id, prompt_pair.1.session_id);
    assert_eq!(prompt_pair.0.model, prompt_pair.1.model);
    assert_eq!(prompt_pair.0.originator, prompt_pair.1.originator);
    assert_eq!(prompt_pair.0.ctx_id, prompt_pair.1.ctx_id);
    let identity_section = format!("## Agent identity\n\nYour agent id is `{agent_id}`.");
    assert_eq!(prompt.agent_id.as_str(), agent_id);
    assert!(prompt.system_prompt.trim_end().ends_with(&identity_section));
    assert_eq!(prompt.system_prompt.matches(&identity_section).count(), 1);

    h.shutdown().expect("shutdown");
}

/// Regression: `agent_id` is a first-class system prompt template variable, not
/// a harness-level post-render prefix. A role `prompt_override` can therefore
/// place the id in custom wording, and the built-in default identity section is
/// not layered on top of custom templates.
#[test]
fn prompt_override_template_can_place_agent_id_without_default_duplication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let selected_role = h.selected_role.clone();
    h.system_prompt_templates.insert(
        "custom-template".to_owned(),
        "Custom identity placement: {{agent_id}}\n\nRole={{role.name}}".to_owned(),
    );
    h.available_roles
        .entry(selected_role.clone())
        .or_default()
        .prompt_override = Some("custom-template".to_owned());

    h.submit_user_prompt("s1".into(), "hello".to_owned())
        .expect("submit first user prompt");

    let agent_id = h
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.agent_id.as_deref())
        .expect("first prompt minted agent id");
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        prompt.system_prompt,
        format!("Custom identity placement: {agent_id}\n\nRole={selected_role}")
    );
    assert_eq!(prompt.system_prompt.matches(agent_id).count(), 1);
    assert!(!prompt.system_prompt.contains("## Agent identity"));

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_first_user_prompt_publishes_replayable_agent_target() {
    // Regression: if the first prompt queues before the provider/model is ready,
    // the agent id must already exist and be carried on the transient queued
    // event and in-memory replay so a live or late UI can select the same
    // conversation before dispatch. Queue lifecycle events are intentionally not
    // durable session-store facts.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.provider_model_routes.clear();
    h.provider_model_info.clear();
    h.available_models.clear();
    h.selected_model = None;

    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        parent_agent: None,
        session_id: "s1".into(),
        role: h.selected_role.clone(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: Some("hello while cold".to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    })
    .expect("create agent with queued first prompt");

    let agent_id = h
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.agent_id.as_deref())
        .expect("queued first prompt minted agent id")
        .to_owned();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptQueued(queued)
            if queued.agent_id.as_str() == agent_id.as_str()
    )));
    let default_conversation = h
        .agents
        .get(&test_user_agent(&h))
        .expect("default conversation");
    assert_eq!(default_conversation.pending_prompts.len(), 1);
    assert_eq!(
        default_conversation.pending_prompts[0].text,
        "hello while cold"
    );
    assert!(
        loaded_agent_events(&h, "s1")
            .iter()
            .all(|event| !matches!(event, Event::AgentPromptQueued(_))),
        "queued prompts are transient and must not be persisted"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();
    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("agent.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut queued = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(Event::AgentPromptQueued(event)) = inner {
            queued.push(event);
        }
    }
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "hello while cold");
    assert_eq!(queued[0].agent_id.as_str(), agent_id.as_str());

    h.shutdown().expect("shutdown");
}

/// `UiCreateAgent.metadata` must be embedded in the durable creation fact so
/// replay restores shell cwd before `session.agent_loaded`.
#[test]
fn ui_create_agent_embeds_shell_cwd_metadata_in_agent_started() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cwd = std::path::PathBuf::from("/tmp/tau-ui-cwd");

    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        parent_agent: None,
        session_id: "s1".into(),
        role: h.selected_role.clone(),
        model_override: None,
        metadata: vec![tau_proto::AgentInitialMetadata {
            key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
            value: CborValue::Text(cwd.display().to_string()),
            inheritable: true,
        }],
        initial_prompt: None,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    })
    .expect("create agent");

    assert!(event_log_events(&h).iter().any(|event| {
        let Event::AgentStarted(started) = event else {
            return false;
        };
        started.metadata.iter().any(|item| {
            item.key.as_str() == "ext_core-shell_cwd"
                && item.value == CborValue::Text(cwd.display().to_string())
                && item.inheritable
        })
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn resume_ignores_later_side_queued_or_steered_default_agent_candidates() {
    // Regression: queued/steered events do not carry an originator, so a later
    // side-conversation durable event must not steal the default conversation's
    // agent binding during cold resume.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    {
        let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
        let mut sessions = tau_core::SessionStore::open(&sessions_dir).expect("session store");
        for agent_id in ["engineer_default", "worker_steered"] {
            sessions
                .append_session_event(
                    "s1",
                    None,
                    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                        session_id: "s1".into(),
                        agent_id: crate::parse_agent_id(agent_id),
                        ephemeral: false,
                    }),
                )
                .expect("seed session membership");
        }
        drop(sessions);

        let mut agents = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
        agents
            .append_agent_event(
                "engineer_default",
                None,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    agent_id: tau_proto::AgentId::parse("engineer_default").expect("agent id"),
                    text: "default prompt".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    submission_source: Default::default(),
                    display_name: None,
                    ctx_id: None,
                }),
            )
            .expect("seed default prompt");
        agents
            .append_agent_event(
                "worker_steered",
                None,
                Event::AgentPromptSteered(AgentPromptSteered {
                    agent_id: tau_proto::AgentId::parse("worker_steered").expect("agent id"),
                    text: "side steered".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    ctx_id: None,
                }),
            )
            .expect("seed side steered prompt");
    }

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert_eq!(
        h.agents
            .get(&test_user_agent(&h))
            .and_then(|conversation| conversation.agent_id.as_deref()),
        Some("engineer_default")
    );
    assert_eq!(
        h.agent_routes.get("engineer_default"),
        Some(&test_user_agent(&h))
    );
    assert_ne!(
        h.agent_routes.get("worker_steered"),
        Some(&test_user_agent(&h))
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn resume_rehydrates_delegated_agent_role_from_agent_log() {
    // Regression: resumed delegated agents must keep the role selected when the
    // delegate was created. Otherwise a targeted follow-up after cold resume
    // falls back to the harness's currently selected interactive role.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.selected_model = Some("test/model".into());
        h.handle_start_agent_request(
            "conn-delegate",
            StartAgentRequest {
                parent_agent: None,
                query_id: "q-role".to_owned(),
                instruction: "side task".to_owned(),
                role: Some("engineer-senior".to_owned()),
                input_stats: tau_proto::ToolUseStats::default(),
                tool_call_id: Some("delegate-call".into()),
                task_name: None,
            },
        )
        .expect("start delegate");
        let cid = ext_query_cid(&h, "q-role").expect("delegated conversation");
        let agent_id = h
            .agents
            .get(&cid)
            .and_then(|conversation| conversation.agent_id.clone())
            .expect("delegated agent id");
        h.shutdown().expect("shutdown");
        agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    h.selected_role = "engineer-junior".to_owned();
    let cid = h
        .agent_routes
        .get(&agent_id)
        .cloned()
        .expect("resumed delegated conversation");
    assert_eq!(
        h.agents
            .get(&cid)
            .and_then(|conversation| conversation.role.as_deref()),
        Some("engineer-senior")
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn resume_rehydrates_default_agent_conversation_from_durable_routing() {
    // Regression: after a cold resume the UI may know the selected agent id from
    // replay and send targeted prompts. The harness must rebuild the live
    // agent_id -> default conversation map from durable session events.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.selected_model = Some("test/model".into());
        h.submit_user_prompt("s1".into(), "hello".to_owned())
            .expect("submit first prompt");
        let agent_id = h
            .agents
            .get(&test_user_agent(&h))
            .and_then(|conversation| conversation.agent_id.clone())
            .expect("first prompt minted agent id");
        h.shutdown().expect("shutdown");
        agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert_eq!(h.agent_routes.get(&agent_id), Some(&test_user_agent(&h)));
    h.shutdown().expect("shutdown");
}

fn text_part(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::Message(message) => message.content.first().map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        }),
        ContextItem::ToolResult(result) => match &result.output.raw {
            CborValue::Text(text) => Some(text.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn tool_call_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolCall(call) => Some(call.call_id.as_str()),
        _ => None,
    }
}

fn tool_result_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolResult(result) => Some(result.call_id.as_str()),
        _ => None,
    }
}

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

fn provider_text_response(
    spid: &AgentPromptId,
    agent_id: tau_proto::AgentId,
    text: &str,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: spid.clone(),
        agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
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
    }
}

fn provider_repetition_response(
    spid: &AgentPromptId,
    agent_id: tau_proto::AgentId,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: spid.clone(),
        agent_id,
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::RepetitionDetected,
        error: Some("provider stream repetition detected".to_owned()),
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn seed_prior_user_message(state_dir: &Path, text: &str) {
    seed_prior_user_message_at(state_dir, text, tau_proto::UnixMicros::now());
}

fn seed_prior_user_message_at(state_dir: &Path, text: &str, recorded_at: tau_proto::UnixMicros) {
    seed_main_agent_loaded(state_dir);
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event_at(
            "main",
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                text: text.to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
            recorded_at,
        )
        .expect("seed prior user message");
}

fn seed_main_agent_loaded(state_dir: &Path) {
    seed_agent_loaded(state_dir, "s1", "main");
}

fn seed_agent_loaded(state_dir: &Path, session_id: &str, agent_id: &str) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            session_id,
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: session_id.into(),
                agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
}

fn context_text_count(prompt: &AgentPromptCreated, text: &str) -> usize {
    prompt
        .context
        .flatten()
        .iter()
        .filter(|item| text_part(item) == Some(text))
        .count()
}

fn restore_notice_context_text(prompt: &AgentPromptCreated) -> Option<String> {
    prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .find(|text| is_restore_notice_prompt_text(text))
        .map(str::to_owned)
}

fn restore_notice_context_count(prompt: &AgentPromptCreated) -> usize {
    prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .filter(|text| is_restore_notice_prompt_text(text))
        .count()
}

fn restore_notice_event_count(h: &Harness) -> usize {
    loaded_agent_events(h, "s1")
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentPromptSubmitted(prompt)
                    if prompt.message_class.is_internal()
                        && is_restore_notice_prompt_text(&prompt.text)
            )
        })
        .count()
}

fn restored_background_notice(call_id: &str) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn seed_background_placeholder(state_dir: &Path, call_id: &str, tool_name: &str) {
    seed_background_placeholder_for_agent(state_dir, "main", call_id, tool_name);
}

fn seed_background_placeholder_for_agent(
    state_dir: &Path,
    agent_id: &str,
    call_id: &str,
    tool_name: &str,
) {
    seed_agent_loaded(state_dir, "s1", agent_id);
    let parsed_agent_id = tau_proto::AgentId::parse(agent_id).expect("agent id");
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: parsed_agent_id.clone(),
                text: format!("run {tool_name}"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed prior user message");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: format!("sp-{call_id}").into(),
                agent_id: parsed_agent_id,
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new(tool_name),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
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
            }),
        )
        .expect("seed background tool call");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text(format!(
                    "{}: true\n\nTool call `{call_id}` is running in the background.",
                    tau_proto::TAU_INTERNAL_HEADER_NAME
                )),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background placeholder");
}

fn seed_background_result(state_dir: &Path, call_id: &str, tool_name: &str, output: &str) {
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text(output.to_owned()),
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background result");
}

fn seed_background_error(state_dir: &Path, call_id: &str, tool_name: &str, message: &str) {
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                message: message.to_owned(),
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background error");
}

fn agent_event_count(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> usize {
    h.agent_routes
        .keys()
        .filter_map(|agent_id| h.agent_store.agent_events(agent_id).ok())
        .flatten()
        .filter(|entry| matches_event(&entry.event))
        .count()
}

fn background_error_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(
        h,
        |event| matches!(event, Event::ToolBackgroundError(error) if error.call_id.as_str() == call_id),
    )
}

fn background_result_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(
        h,
        |event| matches!(event, Event::ToolBackgroundResult(result) if result.call_id.as_str() == call_id),
    )
}

fn tool_result_count(h: &Harness, call_id: &str) -> usize {
    event_log_count(
        h,
        |event| matches!(event, Event::ToolResult(result) if result.call_id.as_str() == call_id),
    )
}

fn background_placeholder_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(h, |event| {
        matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == call_id
                    && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        )
    })
}

fn event_log_contains(h: &Harness, source: &str, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && matches_event(&entry.event) {
            return true;
        }
    }
    false
}

fn event_log_position(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> Option<u64> {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return Some(entry.seq.get());
        }
    }
    None
}

fn event_log_position_after(
    h: &Harness,
    after_seq: u64,
    matches_event: impl Fn(&Event) -> bool,
) -> Option<u64> {
    let mut seq = crate::event_log::EventLogSeq::new(after_seq + 1);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return Some(entry.seq.get());
        }
    }
    None
}

fn event_log_contains_any_source(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return true;
        }
    }
    false
}

fn event_log_count(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> usize {
    let mut count = 0;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            count += 1;
        }
    }
    count
}

fn shared_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn exclusive_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        ..shared_test_tool_spec(name)
    }
}

fn scheduled_test_tool_spec(
    name: &str,
    background_support: tau_proto::BackgroundSupport,
) -> ToolSpec {
    ToolSpec {
        background_support: Some(background_support),
        ..shared_test_tool_spec(name)
    }
}

fn setup_routed_test_tool_call(call_id: &str, tool_name: &str) -> (TempDir, Harness) {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-owner");
    let _ = connect_test_tool(&mut h, "conn-wrong");
    h.registry
        .register("conn-owner", shared_test_tool_spec(tool_name));

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = format!("sp-{call_id}").into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("tool call routed");
    assert_eq!(
        h.pending_tool_providers
            .get(call_id)
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );

    (td, h)
}

fn tool_invoke_call_ids(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<String> {
    events
        .lock()
        .expect("sink mutex")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::ToolStarted(invoke)) => Some(invoke.call_id.to_string()),
            _ => None,
        })
        .collect()
}

/// Invalid model arguments must be rejected before tool dispatch. The harness
/// still emits a logical `ToolError` for user-visible UI state and a
/// provider-facing error for the model, but no `ToolRequest`/`ToolStarted`.
#[test]
fn invalid_tool_arguments_are_rejected_before_logical_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-strict-tool");
    let mut spec = shared_test_tool_spec("strict_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "allowed": { "type": "string" }
        },
        "required": ["allowed"],
        "additionalProperties": false
    }));
    spec.examples = vec![tau_proto::ToolExample {
        id: "allowed-string".to_owned(),
        title: Some("Allowed string".to_owned()),
        arguments: CborValue::Map(vec![(
            CborValue::Text("allowed".to_owned()),
            CborValue::Text("ok".to_owned()),
        )]),
        note: Some("Only include schema-declared fields.".to_owned()),
        subcommand: None,
    }];
    h.registry.register("conn-strict-tool", spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-invalid-tool-args".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "bad-args".into(),
            name: ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("allowed".to_owned()),
                    CborValue::Text("ok".to_owned()),
                ),
                (
                    CborValue::Text("extra".to_owned()),
                    CborValue::Text("nope".to_owned()),
                ),
            ]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "bad-args" => {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.request");
            }
            Event::ToolStarted(invoke) if invoke.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.started");
            }
            Event::ToolError(error) if error.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    let provider_error = provider_error.expect("provider tool error");
    assert!(provider_error.contains("invalid arguments for tool `strict_tool`"));
    assert!(provider_error.contains("unexpected argument(s): `extra`"));
    assert!(provider_error.contains("allowed fields: `allowed`"));
    assert!(provider_error.contains("Example valid call:"));
    assert!(provider_error.contains("\"allowed\":\"ok\""));
    assert_eq!(logical_events, vec!["tool.error"]);
    assert!(tool_invoke_call_ids(&tool_events).is_empty());

    h.execute_agent_tool_call(
        &cid,
        &crate::harness::AgentToolCall {
            id: "bad-args-repeat".into(),
            name: ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("allowed".to_owned()),
                    CborValue::Text("ok".to_owned()),
                ),
                (
                    CborValue::Text("extra".to_owned()),
                    CborValue::Text("nope".to_owned()),
                ),
            ]),
        },
    )
    .expect("repeat tool call handled");

    let mut repeat_error = None;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "bad-args-repeat"
        {
            repeat_error = Some(error.message.clone());
        }
    }
    let repeat_error = repeat_error.expect("repeat provider tool error");
    assert!(repeat_error.contains("invalid arguments for tool `strict_tool`"));
    assert!(!repeat_error.contains("Example valid call:"));

    h.shutdown().expect("shutdown");
}

#[test]
fn invalid_tool_arguments_are_repaired_and_revalidated_before_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-repair-tool");
    let mut spec = shared_test_tool_spec("repair_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer" },
            "flags": { "type": "array", "items": { "type": "boolean" } },
            "optional": { "type": "string" }
        },
        "required": ["count", "flags"],
        "additionalProperties": false
    }));
    h.registry.register("conn-repair-tool", spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-repair-tool-args".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "repair-args".into(),
            name: ToolName::new("repair_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("count".to_owned()),
                    CborValue::Text("42".to_owned()),
                ),
                (CborValue::Text("flags".to_owned()), CborValue::Bool(true)),
                (CborValue::Text("optional".to_owned()), CborValue::Null),
            ]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("provider response handled");

    let mut published_request = None;
    let mut provider_error = None;
    let mut notice = None;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ToolRequest(request) if request.call_id.as_str() == "repair-args" => {
                published_request = Some(request.clone());
            }
            Event::ProviderToolError(error) if error.call_id.as_str() == "repair-args" => {
                provider_error = Some(error.message.clone());
            }
            Event::HarnessNotice(harness_notice)
                if harness_notice
                    .message
                    .contains("Repaired arguments for tool `repair_tool`") =>
            {
                notice = Some(harness_notice.clone());
            }
            _ => {}
        }
    }

    let published_request = published_request.expect("repaired tool request");
    assert_eq!(
        published_request.arguments,
        CborValue::Map(vec![
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer(42.into()),
            ),
            (
                CborValue::Text("flags".to_owned()),
                CborValue::Array(vec![CborValue::Bool(true)]),
            ),
        ])
    );
    assert!(provider_error.is_none());
    assert!(notice.is_some());

    h.shutdown().expect("shutdown");
}

#[test]
fn repaired_tool_arguments_are_rejected_when_revalidation_fails() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-repair-revalidate-tool");
    let mut spec = shared_test_tool_spec("repair_revalidate_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer", "minimum": 1 }
        },
        "required": ["count"],
        "additionalProperties": false
    }));
    h.registry.register("conn-repair-revalidate-tool", spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-repair-revalidate-tool-args".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "repair-revalidate-args".into(),
            name: ToolName::new("repair_revalidate_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("count".to_owned()),
                CborValue::Text("0".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "repair-revalidate-args" =>
            {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "repair-revalidate-args" => {
                logical_events.push("tool.request");
            }
            Event::ToolError(error) if error.call_id.as_str() == "repair-revalidate-args" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    let provider_error = provider_error.expect("provider error");
    assert!(provider_error.contains("invalid arguments for tool `repair_revalidate_tool`"));
    assert!(provider_error.contains("$.count: expected integer, got string"));
    assert_eq!(logical_events, vec!["tool.error"]);

    h.shutdown().expect("shutdown");
}

#[test]
fn rendered_tool_definitions_do_not_include_examples() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");
    let mut spec = shared_test_tool_spec("example_tool");
    spec.description = Some("visible description".to_owned());
    spec.examples = vec![tau_proto::ToolExample {
        id: "repair".to_owned(),
        title: Some("Repair".to_owned()),
        arguments: CborValue::Map(vec![(
            CborValue::Text("field".to_owned()),
            CborValue::Text("value".to_owned()),
        )]),
        note: Some("This should not be in prompt tool definitions.".to_owned()),
        subcommand: None,
    }];

    let definitions = h.tool_definitions_from_specs(&[spec]);
    let rendered = serde_json::to_string(&definitions).expect("definitions serialize");

    assert!(rendered.contains("visible description"));
    assert!(!rendered.contains("examples"));
    assert!(!rendered.contains("Repair"));
    assert!(!rendered.contains("This should not be in prompt"));
}

#[test]
fn invalid_tool_example_registration_is_rejected_with_notice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let _events = connect_test_tool(&mut h, "conn-invalid-example");
    let mut spec = shared_test_tool_spec("invalid_example_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
        "additionalProperties": false
    }));
    spec.examples = vec![tau_proto::ToolExample {
        id: "bad".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Integer(1.into()),
        )]),
        note: None,
        subcommand: None,
    }];

    h.handle_extension_event_inner(
        "conn-invalid-example",
        Event::ToolRegister(tau_proto::ToolRegister {
            tool: spec,
            tool_group: None,
            prompt_fragment: None,
        }),
    )
    .expect("registration handled");

    assert!(h.registry.providers_for("invalid_example_tool").is_empty());
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Critical
                && notice.always_show
                && notice.message.contains("Rejected tool registration")
                && notice.message.contains("invalid example `bad`")
                && notice.message.contains("$.path: expected string")
    )));
}

#[test]
fn shown_tool_failure_examples_are_session_and_agent_scoped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.shown_tool_failure_examples.insert((
        cid.clone(),
        ToolName::new("example_tool"),
        "hint".to_owned(),
    ));

    h.remove_agent(&cid);

    assert!(h.shown_tool_failure_examples.is_empty());

    let cid = ensure_test_user_agent(&mut h);
    h.shown_tool_failure_examples
        .insert((cid, ToolName::new("example_tool"), "hint".to_owned()));
    h.switch_session(
        tau_proto::SessionId::new("session-after-example-hint"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");

    assert!(h.shown_tool_failure_examples.is_empty());
}

fn loop_guard_tool_error(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    }
}

#[test]
fn loop_guard_detects_repeated_assistant_text_once_then_blocks() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(conv.pending_prompts[0].text.contains("Loop guard:"));
    assert!(!conv.loop_guard.stop_automatic_continuation());

    h.mark_loop_guard_breakers_dispatched(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(conv.loop_guard.stop_automatic_continuation());
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_repeated_assistant_text_flows_through_provider_responses() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    for idx in 0..3 {
        let spid: AgentPromptId = format!("sp-loop-text-{idx}").into();
        seed_agent_thinking(&mut h, &cid, spid.as_str());
        h.prompt_agents.insert(spid.clone(), cid.clone());
        h.handle_provider_response_finished(provider_text_response(
            &spid,
            tau_proto::AgentId::parse("main").expect("agent id"),
            text,
        ))
        .expect("response handled");
    }

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("Loop guard:")
    )));
    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .pending_prompts
            .is_empty()
    );

    let spid = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_text_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
        text,
    ))
    .expect("response handled");

    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_provider_repetition_response_queues_pivot_then_blocks() {
    // Provider-side stream repetition is trusted only as a loop-guard trigger:
    // the provider's error is display text, while the harness uses a fixed pivot
    // reason and blocks only after the breaker was tried.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);

    let spid: AgentPromptId = "sp-provider-repetition-1".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("provider detected a tight exact stream repetition")
    )));
    let spid = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Warning
                && notice.always_show
                && notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn repetition_response_with_output_items_is_cleared_before_persisting() {
    // The harness enforces the repetition-detected empty-output invariant at the
    // provider boundary so a buggy provider cannot smuggle text or tool calls in
    // a loop-guard terminal response.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-provider-repetition-malformed".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    let mut response =
        provider_repetition_response(&spid, tau_proto::AgentId::parse("main").expect("agent id"));
    response.output_items = provider_text_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
        "this text must be discarded",
    )
    .output_items;

    h.handle_provider_response_finished(response)
        .expect("response handled");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if finished
                .output_items
                .iter()
                .any(|item| matches!(item, ContextItem::Message(_)))
    )));
}

#[test]
fn side_agent_repetition_response_propagates_error_result() {
    // Empty repetition/error provider responses from extension-originated side
    // agents must not look like successful empty delegation results.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let frames = connect_test_client(&mut h, "conn-side", tau_proto::ClientKind::External);

    h.handle_start_agent_request(
        "conn-side",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-repetition".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");
    let (side_spid, side_cid) = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default").then(|| (spid.clone(), prompt_cid.clone()))
        })
        .expect("side prompt id");
    let mut response = provider_repetition_response(
        &side_spid,
        tau_proto::AgentId::parse("side").expect("agent id"),
    );
    response.originator = tau_proto::PromptOriginator::Extension {
        name: "conn-side".into(),
        query_id: "q-repetition".to_owned(),
    };
    response.error = Some("provider stream repetition detected".to_owned());

    h.handle_provider_response_finished(response)
        .expect("side response handled");

    assert!(!h.agents.contains_key(&side_cid));
    let result = frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-repetition" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("start-agent result routed");
    assert!(result.text.is_empty());
    assert_eq!(
        result.error.as_deref(),
        Some("provider stream repetition detected")
    );
}

#[test]
fn side_agent_error_response_propagates_error_result() {
    // Plain provider errors with no assistant text should also reach the
    // StartAgentResult error field instead of becoming silent empty text.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let frames = connect_test_client(&mut h, "conn-side-error", tau_proto::ClientKind::External);

    h.handle_start_agent_request(
        "conn-side-error",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-error".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");
    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("side").expect("agent id"),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("provider failed".to_owned()),
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-side-error".into(),
            query_id: "q-error".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response handled");

    let result = frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-error" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("start-agent result routed");
    assert!(result.text.is_empty());
    assert_eq!(result.error.as_deref(), Some("provider failed"));
}

#[test]
fn loop_guard_detects_repeated_same_failing_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "missing file"),
        );
    }

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(
        conv.pending_prompts[0]
            .text
            .contains("repeated identical failing tool call")
    );
}

#[test]
fn loop_guard_repeated_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt"].into_iter().enumerate() {
        let call = crate::harness::AgentToolCall {
            id: format!("call-{idx}").into(),
            name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.to_owned()),
            )]),
        };
        h.remember_tool_call_loop_signature(&cid, &call);
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(call.id.as_str(), "read", "generic failure"),
        );
    }

    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_production_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                id: format!("distinct-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(format!("file-{idx}.txt")),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .pending_prompts
            .is_empty()
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("Loop guard:")
    )));

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                id: format!("same-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("same.txt".to_owned()),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("repeated identical failing tool call")
    )));
}

#[test]
fn loop_guard_progress_reset_preserves_in_flight_argument_signatures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        h.remember_tool_call_loop_signature(
            &cid,
            &crate::harness::AgentToolCall {
                id: format!("call-{idx}").into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(path.to_owned()),
                )]),
            },
        );
    }

    h.reset_loop_guard_for_progress(&cid);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "generic failure"),
        );
    }

    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_detects_consecutive_different_tool_failures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(
        conv.pending_prompts[0]
            .text
            .contains("several consecutive tool failures")
    );
}

#[test]
fn loop_guard_same_batch_failures_do_not_block_before_breaker_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(!conv.loop_guard.stop_automatic_continuation());
}

#[test]
fn loop_guard_detects_abab_suffix() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-a", "read", "first distinct failure"),
    );
    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-b", "read", "first distinct failure"),
    );

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
    assert!(conv.pending_prompts[0].text.contains("A/B/A/B"));
}

#[test]
fn loop_guard_resets_on_user_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.reset_loop_guard_for_progress(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h.agents.get(&cid).expect("agent");
    assert!(conv.pending_prompts.is_empty());
}

#[test]
fn loop_guard_resets_when_user_prompt_is_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
        .expect("publish user prompt");

    let conv = h.agents.get(&cid).expect("agent");
    assert!(
        !conv
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_reset_removes_pending_breaker_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    assert!(
        h.agents
            .get(&cid)
            .expect("agent")
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );

    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new input".to_owned()))
        .expect("publish user prompt");

    let conv = h.agents.get(&cid).expect("agent");
    assert!(
        !conv
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_resets_when_user_prompt_is_queued() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.agents.get_mut(&cid).expect("agent").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-loop-queued-reset".into(),
    };
    h.record_assistant_loop_signature(
        &cid,
        Some("I will keep trying the same plan without taking any tool action or making progress."),
    );

    let submission = h
        .submit_prompt_to_agent(
            h.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.pending_prompts.len(), 1);
}

#[test]
fn loop_guard_queued_user_prompt_removes_pending_breaker() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.agents.get_mut(&cid).expect("agent").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-loop-queued-breaker-reset".into(),
    };

    let submission = h
        .submit_prompt_to_agent(
            h.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    let conv = h.agents.get(&cid).expect("agent");
    assert!(
        !conv
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_folding_user_prompt_drops_stale_pivot_from_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h.agents.get_mut(&cid).expect("agent");
        conv.pending_prompts
            .push_back(PendingPrompt::user("queued user input".to_owned()));
    }
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.fold_pending_prompts_as_steered(&cid);

    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSteered(steered) if steered.text.contains("Loop guard:")
        )
    }));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "queued user input"
    )));
}

#[test]
fn loop_guard_block_preserves_agent_message_internal_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h.agents.get_mut(&cid).expect("agent");
        conv.loop_guard.mark_cycle_blocked("cycle");
        conv.pending_prompts
            .push_back(PendingPrompt::agent_message_received(
                "external message".to_owned(),
            ));
    }
    seed_tools_running(&mut h, &cid, vec!["done-call".into()]);

    h.maybe_complete_agent_turn_for(&cid, "done-call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "external message"
    )));
}

#[test]
fn loop_guard_resets_on_successful_terminal_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );

    h.publish_terminal_tool_result(
        Some(&cid),
        None,
        tau_proto::ToolResult {
            call_id: "ok-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.loop_guard.consecutive_tool_failures(), 0);
    assert!(conv.pending_prompts.is_empty());
}

#[test]
fn loop_guard_resets_on_successful_background_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );
    h.tool_agents.insert("bg-call".into(), cid.clone());
    h.pending_tools.insert(
        "bg-call".into(),
        PendingTool {
            name: ToolName::new("read"),
            internal_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
        },
    );

    h.handle_background_tool_result(
        "conn-bg",
        tau_proto::ToolResult {
            call_id: "bg-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h.agents.get(&cid).expect("agent");
    assert_eq!(conv.loop_guard.consecutive_tool_failures(), 0);
    assert!(conv.pending_prompts.is_empty());
}

#[test]
fn loop_guard_resets_on_agent_head_move() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("seed".to_owned()))
        .expect("publish seed");
    let node_id = h.agents.get(&cid).expect("agent").head.expect("agent head");
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: tau_proto::AgentHead::Node(node_id),
        }),
    );

    let conv = h.agents.get(&cid).expect("agent");
    assert!(
        !conv
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

/// Ensures unavailable-tool diagnostics distinguish a misspelled/unknown tool
/// from a registered tool disabled by policy, and give a near-name suggestion
/// only for the unknown-tool case where the model can retry mechanically.
#[test]
fn unavailable_tool_errors_are_actionable_for_unknown_and_disabled_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.selected_role = "restricted".to_owned();
    h.available_roles.insert(
        "restricted".to_owned(),
        tau_config::settings::AgentRole {
            disable_tools: vec![ToolName::new("disabled_tool")],
            ..Default::default()
        },
    );

    h.registry
        .register("conn-readable", shared_test_tool_spec("readable"));
    h.registry
        .register("conn-disabled", shared_test_tool_spec("disabled_tool"));

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-unavailable-tool-errors".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "unknown-tool".into(),
                name: ToolName::new("readble"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "disabled-tool".into(),
                name: ToolName::new("disabled_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut unknown_error = None;
    let mut disabled_error = None;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "unknown-tool" => {
                unknown_error = Some(error.message.clone());
            }
            Event::ProviderToolError(error) if error.call_id.as_str() == "disabled-tool" => {
                disabled_error = Some(error.message.clone());
            }
            _ => {}
        }
    }

    let unknown_error = unknown_error.expect("unknown tool provider error");
    assert!(unknown_error.contains("Tool `readble` is not available."));
    assert!(unknown_error.contains("Did you mean `readable`?"));
    let disabled_error = disabled_error.expect("disabled tool provider error");
    assert!(
        disabled_error
            .contains("Tool `disabled_tool` was not in the tool set advertised for this prompt."),
        "unexpected disabled error: {disabled_error}"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures a registered tool that is disabled by the current live role/model is
/// reported as disabled, not as unknown, when the call is not tied to a prompt
/// snapshot.
#[test]
fn current_role_disabled_tool_error_names_role_model_authority() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.selected_role = "restricted".to_owned();
    h.available_roles.insert(
        "restricted".to_owned(),
        tau_config::settings::AgentRole {
            disable_tools: vec![ToolName::new("disabled_tool")],
            ..Default::default()
        },
    );
    h.registry
        .register("conn-disabled", shared_test_tool_spec("disabled_tool"));
    let cid = ensure_test_user_agent(&mut h);

    h.execute_agent_tool_call(
        &cid,
        &AgentToolCall {
            id: "disabled-current-role".into(),
            name: ToolName::new("disabled_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
    )
    .expect("disabled direct call handled");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == "disabled-current-role"
                && error.message.contains(
                    "Tool `disabled_tool` exists, but is disabled for the current role/model."
                )
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures unknown-tool suggestions for a provider response use the prompt's
/// advertised tool snapshot rather than the current role surface, which can
/// change while the model is thinking.
#[test]
fn unknown_tool_suggestion_uses_prompt_tool_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.registry
        .register("conn-current", shared_test_tool_spec("current_tool"));

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-snapshot-tool-suggestion".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.prompt_tool_specs
        .insert(spid.clone(), vec![shared_test_tool_spec("snapshot_tool")]);
    h.prompt_tool_call_prompts
        .insert("snapshot-typo".into(), spid.clone());
    h.prompt_tool_call_prompts
        .insert("registered-not-snapshot".into(), spid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "snapshot-typo".into(),
                name: ToolName::new("snapshot_too"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "registered-not-snapshot".into(),
                name: ToolName::new("current_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut registered_error = None;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "snapshot-typo"
        {
            provider_error = Some(error.message.clone());
        }
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "registered-not-snapshot"
        {
            registered_error = Some(error.message.clone());
        }
    }

    let provider_error = provider_error.expect("provider tool error");
    assert!(provider_error.contains("Did you mean `snapshot_tool`?"));
    assert!(!provider_error.contains("current_tool"));
    let registered_error = registered_error.expect("registered tool outside snapshot error");
    assert!(
        registered_error
            .contains("Tool `current_tool` was not in the tool set advertised for this prompt.")
    );
    assert!(!registered_error.contains("current role/model"));

    h.shutdown().expect("shutdown");
}

/// If an old prompt calls a tool that was advertised with a strict schema but
/// whose provider has since disappeared, provider availability must be checked
/// before schema validation. The model should receive the accurate NoProvider
/// error instead of a misleading invalid-arguments error for a tool that cannot
/// run anymore.
#[test]
fn old_prompt_missing_provider_wins_over_strict_schema_validation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-strict-old-tool");
    let mut spec = shared_test_tool_spec("strict_old_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "allowed": { "type": "string" }
        },
        "required": ["allowed"],
        "additionalProperties": false
    }));
    h.registry.register("conn-strict-old-tool", spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-strict-old-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.registry.unregister_connection("conn-strict-old-tool");

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "missing-strict".into(),
            name: ToolName::new("strict_old_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("extra".to_owned()),
                CborValue::Text("nope".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("old prompt missing provider handled");

    let expected = unavailable_tool_error_message(&ToolName::new("strict_old_tool"));
    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "missing-strict" => {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.request");
            }
            Event::ToolStarted(invoke) if invoke.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.started");
            }
            Event::ToolError(error) if error.call_id.as_str() == "missing-strict" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    assert_eq!(provider_error.as_deref(), Some(expected.as_str()));
    assert_eq!(logical_events, vec!["tool.request", "tool.error"]);
    assert!(tool_invoke_call_ids(&tool_events).is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_with_multiple_inflight_tools_cleans_up_all_calls() {
    // Regression: disconnect cleanup must unregister the provider before it
    // synthesizes terminal errors, and every in-flight call for that provider
    // must be closed exactly once.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-dead-tool");
    h.registry
        .register("conn-dead-tool", exclusive_test_tool_spec("dead_slow"));

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-dead-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run two slow tools".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "running-call".into(),
                name: ToolName::new("dead_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-call".into(),
                name: ToolName::new("dead_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned(), "queued-call".to_owned()]
    );
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.handle_disconnect("conn-dead-tool");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned(), "queued-call".to_owned()]
    );
    assert!(h.registry.providers_for("dead_slow").is_empty());
    assert!(h.tool_turn.is_empty());
    assert!(!h.pending_tool_providers.contains_key("running-call"));
    assert!(!h.pending_tool_providers.contains_key("queued-call"));

    let running: ToolCallId = "running-call".into();
    let queued: ToolCallId = "queued-call".into();
    let running_message = extension_disconnected_tool_call_error_message(&running);
    let queued_message = extension_disconnected_tool_call_error_message(&queued);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "running-call"
                && error.message == running_message
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "queued-call"
                && error.message == queued_message
    )));

    h.shutdown().expect("shutdown");
}

fn final_tool_result(call_id: &str, tool_name: &str, text: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn wait_no_args_call(call_id: &str) -> AgentToolCall {
    AgentToolCall {
        id: call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    }
}

fn tool_error(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn tool_progress(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolProgress {
    tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        message: Some(message.to_owned()),
        progress: None,
        display: None,
    }
}

fn ext_query(query_id: &str) -> StartAgentRequest {
    StartAgentRequest {
        parent_agent: None,
        query_id: query_id.to_owned(),
        instruction: format!("instruction {query_id}"),
        role: None,
        input_stats: tau_proto::ToolUseStats::default(),
        tool_call_id: None,
        task_name: None,
    }
}

fn provider_model_info(
    id: tau_proto::ModelId,
    context_window: u64,
) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id,
        display_name: None,
        tags: Vec::new(),
        default_affinity: 0,
        context_window,
        efforts: vec![tau_proto::Effort::Off, tau_proto::Effort::High],
        verbosities: vec![tau_proto::Verbosity::Low, tau_proto::Verbosity::High],
        thinking_summaries: vec![
            tau_proto::ThinkingSummary::Off,
            tau_proto::ThinkingSummary::Auto,
        ],
        supports_compaction: false,
    }
}

fn set_available_provider_models(
    h: &mut Harness,
    models: impl IntoIterator<Item = tau_proto::ProviderModelInfo>,
) {
    let models: Vec<_> = models.into_iter().collect();
    h.available_models = models.iter().map(|info| info.id.clone()).collect();
    h.provider_model_info = models
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect();
}

fn ext_query_cid(h: &Harness, query_id: &str) -> Option<AgentId> {
    h.agents.iter().find_map(|(cid, conv)| {
        matches!(
            &conv.originator,
            tau_proto::PromptOriginator::Extension { query_id: id, .. } if id == query_id
        )
        .then_some(cid.clone())
    })
}

/// Regression: a backgrounded call remains tracked after its synthetic
/// placeholder closes the foreground, while later tool calls dispatch normally.
/// The real background result must clear only the actual-running state.
#[test]
fn background_result_clears_actual_running_call_without_blocking_later_tool() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-result-drain");
    h.registry.register(
        "conn-bg-result-drain",
        scheduled_test_tool_spec("bg_update", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-result-drain",
        scheduled_test_tool_spec("queued_update", tau_proto::BackgroundSupport::Never),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-result-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-update-running".into(),
                name: ToolName::new("bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update".into(),
                name: ToolName::new("queued_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned(), "queued-update".to_owned()]
    );
    assert_eq!(background_placeholder_count(&h, "bg-update-running"), 1);
    assert!(h.tool_turn.is_backgrounded(&"bg-update-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        "conn-bg-result-drain",
        Event::ToolResult(final_tool_result(
            "bg-update-running",
            "bg_update",
            "background output",
        )),
    )
    .expect("background result accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["bg-update-running".to_owned(), "queued-update".to_owned()]
    );
    assert_eq!(background_result_count(&h, "bg-update-running"), 1);
    assert!(!h.tool_turn.is_backgrounded(&"bg-update-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(!h.pending_tool_providers.contains_key("bg-update-running"));
    assert!(h.pending_tool_providers.contains_key("queued-update"));

    h.shutdown().expect("shutdown");
}

/// Regression: background errors clear the same actual-running state as
/// background results, without affecting unrelated tool calls that already
/// dispatched.
#[test]
fn background_error_clears_actual_running_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-error-drain");
    h.registry.register(
        "conn-bg-error-drain",
        scheduled_test_tool_spec("bg_exclusive", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-error-drain",
        scheduled_test_tool_spec(
            "queued_update_after_error",
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-error-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-running".into(),
                name: ToolName::new("bg_exclusive"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-error".into(),
                name: ToolName::new("queued_update_after_error"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-running".to_owned(),
            "queued-update-after-error".to_owned(),
        ]
    );
    assert_eq!(background_placeholder_count(&h, "bg-exclusive-running"), 1);
    assert!(h.tool_turn.is_backgrounded(&"bg-exclusive-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        "conn-bg-error-drain",
        Event::ToolError(tool_error(
            "bg-exclusive-running",
            "bg_exclusive",
            "background failure",
        )),
    )
    .expect("background error accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-running".to_owned(),
            "queued-update-after-error".to_owned(),
        ]
    );
    assert_eq!(background_error_count(&h, "bg-exclusive-running"), 1);
    assert!(!h.tool_turn.is_backgrounded(&"bg-exclusive-running".into()));
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(
        !h.pending_tool_providers
            .contains_key("bg-exclusive-running")
    );
    assert!(
        h.pending_tool_providers
            .contains_key("queued-update-after-error")
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a cancelled backgrounded call clears actual-running state
/// and publishes a background error instead of an invalid terminal
/// cancellation.
#[test]
fn background_cancel_clears_actual_running_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-bg-cancel-drain");
    h.registry.register(
        "conn-bg-cancel-drain",
        scheduled_test_tool_spec("bg_exclusive_cancel", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-cancel-drain",
        scheduled_test_tool_spec(
            "queued_update_after_cancel",
            tau_proto::BackgroundSupport::Never,
        ),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-cancel-drain".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "bg-exclusive-cancel-running".into(),
                name: ToolName::new("bg_exclusive_cancel"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-update-after-cancel".into(),
                name: ToolName::new("queued_update_after_cancel"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-cancel-running".to_owned(),
            "queued-update-after-cancel".to_owned(),
        ]
    );
    assert_eq!(
        background_placeholder_count(&h, "bg-exclusive-cancel-running"),
        1
    );
    assert!(
        h.tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.handle_extension_event_inner(
        "conn-bg-cancel-drain",
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "bg-exclusive-cancel-running".into(),
            tool_name: ToolName::new("bg_exclusive_cancel"),
            tool_type: tau_proto::ToolType::Function,
        }),
    )
    .expect("background cancellation accepted");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec![
            "bg-exclusive-cancel-running".to_owned(),
            "queued-update-after-cancel".to_owned(),
        ]
    );
    assert!(
        !h.tool_turn
            .is_backgrounded(&"bg-exclusive-cancel-running".into())
    );
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(
        !h.pending_tool_providers
            .contains_key("bg-exclusive-cancel-running")
    );
    assert!(
        h.pending_tool_providers
            .contains_key("queued-update-after-cancel")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled)
            if cancelled.call_id.as_str() == "bg-exclusive-cancel-running"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "bg-exclusive-cancel-running"
                && error.message == "Tool cancelled"
    )));
    assert!(
        h.background_completion_targets
            .contains_key("bg-exclusive-cancel-running")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "bg-exclusive-cancel-running"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: disconnect cleanup can synthesize errors for more than one
/// backgrounded call from the same dead provider without touching unrelated
/// calls that have already dispatched to another provider.
#[test]
fn disconnect_background_errors_do_not_affect_other_inflight_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _dead_events = connect_test_tool(&mut h, "conn-bg-disconnect-batch");
    let live_events = connect_test_tool(&mut h, "conn-bg-disconnect-live");
    h.registry.register(
        "conn-bg-disconnect-batch",
        scheduled_test_tool_spec("dead_bg_shared", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-disconnect-batch",
        scheduled_test_tool_spec("dead_bg_update", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-disconnect-live",
        scheduled_test_tool_spec("live_queued_update", tau_proto::BackgroundSupport::Never),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-disconnect-batch".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-shared".into(),
                name: ToolName::new("dead_bg_shared"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-update".into(),
                name: ToolName::new("dead_bg_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "z-queued-update".into(),
                name: ToolName::new("live_queued_update"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        h.pending_tool_providers
            .get("b-bg-shared")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-batch")
    );
    assert_eq!(
        h.pending_tool_providers
            .get("a-bg-update")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-batch")
    );
    assert_eq!(
        tool_invoke_call_ids(&live_events),
        vec!["z-queued-update".to_owned()]
    );
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert!(h.tool_turn.is_backgrounded(&"a-bg-update".into()));
    assert!(h.tool_turn.is_backgrounded(&"b-bg-shared".into()));

    h.handle_disconnect("conn-bg-disconnect-batch");

    assert_eq!(
        tool_invoke_call_ids(&live_events),
        vec!["z-queued-update".to_owned()]
    );
    assert_eq!(background_error_count(&h, "a-bg-update"), 1);
    assert_eq!(background_error_count(&h, "b-bg-shared"), 1);
    assert!(!h.pending_tool_providers.contains_key("a-bg-update"));
    assert!(!h.pending_tool_providers.contains_key("b-bg-shared"));
    assert_eq!(
        h.pending_tool_providers
            .get("z-queued-update")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-bg-disconnect-live")
    );

    h.shutdown().expect("shutdown");
}

/// Regression: when an idle conversation has more than one backgrounded call on
/// a disconnected provider, the harness must record every synthetic background
/// error before it dispatches the first internal completion prompt back to the
/// model. Dispatching after the first error would let the follow-up miss later
/// failures from the same disconnect batch.
#[test]
fn disconnect_idle_multi_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _dead_events = connect_test_tool(&mut h, "conn-bg-idle-disconnect");
    h.registry.register(
        "conn-bg-idle-disconnect",
        scheduled_test_tool_spec("dead_bg_one", tau_proto::BackgroundSupport::Instant),
    );
    h.registry.register(
        "conn-bg-idle-disconnect",
        scheduled_test_tool_spec("dead_bg_two", tau_proto::BackgroundSupport::Instant),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-idle-disconnect".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-bg-idle".into(),
                name: ToolName::new("dead_bg_one"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-bg-idle".into(),
                name: ToolName::new("dead_bg_two"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("background tool response");

    let followup_spid = match &h
        .agents
        .get(&cid)
        .expect("conversation remains live")
        .turn_state
    {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("expected placeholder follow-up prompt, got {state:?}"),
    };
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_provider_response_finished(provider_text_response(
        &followup_spid,
        agent_id,
        "placeholders observed",
    ))
    .expect("finish placeholder follow-up");
    assert!(matches!(
        h.agents
            .get(&cid)
            .expect("conversation remains live")
            .turn_state,
        AgentTurnState::Idle
    ));

    h.handle_disconnect("conn-bg-idle-disconnect");

    let first_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "a-bg-idle"
        )
    })
    .expect("first background error");
    let second_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id.as_str() == "b-bg-idle"
        )
    })
    .expect("second background error");
    let prompt_after_first_error_seq = event_log_position_after(&h, first_error_seq, |event| {
        matches!(event, Event::AgentPromptCreated(_))
    })
    .expect("background completion follow-up prompt");
    assert!(second_error_seq < prompt_after_first_error_seq);

    h.shutdown().expect("shutdown");
}

/// Regression: a disconnect batch can contain a foreground call that completes
/// the model's tool round plus a later background error. The foreground failure
/// must not complete the agent turn and dispatch a follow-up until the
/// background error from the same dead provider has also been recorded.
#[test]
fn disconnect_mixed_foreground_and_background_errors_dispatch_prompt_after_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let dead_events = connect_test_tool(&mut h, "conn-mixed-disconnect");
    h.registry.register(
        "conn-mixed-disconnect",
        scheduled_test_tool_spec("dead_foreground", tau_proto::BackgroundSupport::Never),
    );
    h.registry.register(
        "conn-mixed-disconnect",
        scheduled_test_tool_spec("dead_background", tau_proto::BackgroundSupport::Instant),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-mixed-disconnect".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "a-foreground-disconnect".into(),
                name: ToolName::new("dead_foreground"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "b-background-disconnect".into(),
                name: ToolName::new("dead_background"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("mixed tool response");

    assert_eq!(
        tool_invoke_call_ids(&dead_events),
        vec![
            "a-foreground-disconnect".to_owned(),
            "b-background-disconnect".to_owned(),
        ]
    );
    assert!(
        h.tool_turn
            .is_backgrounded(&"b-background-disconnect".into())
    );
    assert!(matches!(
        h.agents
            .get(&cid)
            .expect("conversation remains live")
            .turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    h.handle_disconnect("conn-mixed-disconnect");

    let foreground_error_seq = event_log_position(&h, |event| {
        matches!(
            event,
            Event::ToolError(error) if error.call_id.as_str() == "a-foreground-disconnect"
        )
    })
    .expect("foreground synthetic error");
    let background_error_seq = event_log_position_after(&h, foreground_error_seq, |event| {
        matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "b-background-disconnect"
        )
    })
    .expect("background synthetic error");
    let prompt_after_foreground_error_seq =
        event_log_position_after(&h, foreground_error_seq, |event| {
            matches!(event, Event::AgentPromptCreated(_))
        })
        .expect("post-disconnect follow-up prompt");
    assert!(background_error_seq < prompt_after_foreground_error_seq);

    h.shutdown().expect("shutdown");
}

/// A tool result from any connection other than the routed provider must not
/// close the call; otherwise a stale extension can spoof completion and make
/// the real owner look like a duplicate.
#[test]
fn provider_owner_validation_rejects_wrong_tool_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-result-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolResult(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "spoofed output",
        )),
    )
    .expect("wrong result ignored");

    assert!(h.tool_agents.contains_key("owner-result-call"));
    assert_eq!(
        h.pending_tool_providers
            .get("owner-result-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolResult(result) if result.call_id.as_str() == "owner-result-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(!h.tool_agents.contains_key("owner-result-call"));
    assert!(!h.pending_tool_providers.contains_key("owner-result-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "owner-result-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));

    h.shutdown().expect("shutdown");
}

/// A tool error from a non-owner is also ignored so it cannot fail the pending
/// call or remove routing state before the owner reports the real failure.
#[test]
fn provider_owner_validation_rejects_wrong_tool_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-error-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolError(tool_error(
            "owner-error-call",
            "owned_tool",
            "spoofed failure",
        )),
    )
    .expect("wrong error ignored");

    assert!(h.tool_agents.contains_key("owner-error-call"));
    assert_eq!(
        h.pending_tool_providers
            .get("owner-error-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == "owner-error-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolError(tool_error("owner-error-call", "owned_tool", "real failure")),
    )
    .expect("owner error accepted");

    assert!(!h.tool_agents.contains_key("owner-error-call"));
    assert!(!h.pending_tool_providers.contains_key("owner-error-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "owner-error-call" && error.message == "real failure"
    )));

    h.shutdown().expect("shutdown");
}

/// Progress is non-terminal, but it still must come from the routed owner so a
/// wrong extension cannot publish spoofed output into the visible tool block.
#[test]
fn provider_owner_validation_rejects_wrong_tool_progress() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolProgress(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "spoofed progress",
        )),
    )
    .expect("wrong progress ignored");

    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolProgress(progress) if progress.call_id.as_str() == "owner-progress-call"
    )));
    assert!(h.tool_agents.contains_key("owner-progress-call"));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolProgress(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "real progress",
        )),
    )
    .expect("owner progress accepted");

    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "owner-progress-call"
                && progress.message.as_deref() == Some("real progress")
    )));

    h.shutdown().expect("shutdown");
}

/// A terminal cancellation from a non-owner must not poison the tool round
/// before the routed provider returns the real result.
#[test]
fn provider_owner_validation_rejects_wrong_tool_cancelled() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-cancelled-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "owner-cancelled-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
        }),
    )
    .expect("wrong cancellation ignored");

    assert!(h.tool_agents.contains_key("owner-cancelled-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "owner-cancelled-call"
    )));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "owner-cancelled-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(!h.tool_agents.contains_key("owner-cancelled-call"));
    assert!(event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "owner-cancelled-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));

    h.shutdown().expect("shutdown");
}

/// Background terminal events are harness-derived records. Extensions must not
/// be able to inject them directly into an agent log.
#[test]
fn provider_owner_validation_rejects_external_background_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "owner-background-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("spoofed background".to_owned()),
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("wrong background result ignored");

    assert!(h.tool_agents.contains_key("owner-background-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "owner-background-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_background_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-background-error-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-wrong",
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "owner-background-error-call".into(),
            tool_name: ToolName::new("owned_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "spoofed background error".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("wrong background error ignored");

    assert!(h.tool_agents.contains_key("owner-background-error-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "owner-background-error-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_provider_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let _provider = connect_test_client(&mut h, "provider-spoof", tau_proto::ClientKind::Provider);

    h.handle_extension_event_inner(
        "provider-spoof",
        Event::ProviderToolResult(final_tool_result(
            "provider-tool-call",
            "owned_tool",
            "spoofed provider result",
        )),
    )
    .expect("provider tool result ignored");

    assert!(!event_log_contains(&h, "provider-spoof", |event| matches!(
        event,
        Event::ProviderToolResult(result) if result.call_id.as_str() == "provider-tool-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_tool_event_message_emit() {
    let (_td, mut h) = setup_routed_test_tool_call("emit-cancelled-call", "owned_tool");

    h.handle_extension_event(
        "conn-wrong",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: "emit-cancelled-call".into(),
                tool_name: ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            })),
            transient: false,
        })),
    )
    .expect("emitted cancellation ignored");

    assert!(h.tool_agents.contains_key("emit-cancelled-call"));
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "emit-cancelled-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_provider_event_message_emit() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.handle_extension_event(
        "conn-wrong",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: "spoofed-prompt".into(),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                output_items: Vec::new(),
                stop_reason: tau_proto::ProviderStopReason::EndTurn,
                error: None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            })),
            transient: false,
        })),
    )
    .expect("emitted provider event ignored");

    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id.as_str() == "spoofed-prompt"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_late_tool_progress_after_completion() {
    let (_td, mut h) = setup_routed_test_tool_call("late-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolResult(final_tool_result(
            "late-progress-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");
    assert!(!h.tool_agents.contains_key("late-progress-call"));

    h.handle_extension_event_inner(
        "conn-owner",
        Event::ToolProgress(tool_progress(
            "late-progress-call",
            "owned_tool",
            "late progress",
        )),
    )
    .expect("late progress ignored");

    assert!(!event_log_contains(&h, "conn-owner", |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "late-progress-call"
                && progress.message.as_deref() == Some("late progress")
    )));

    h.shutdown().expect("shutdown");
}

/// Cancelling a routed tool publishes the durable broadcast cancellation
/// request and the local terminal `ToolCancelled` event. Extensions observe the
/// event log instead of receiving point-to-point cancellation frames.
#[test]
fn cancel_publishes_tool_cancel_request() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _owner_events = connect_test_tool(&mut h, "conn-cancel-owner");
    h.registry
        .register("conn-cancel-owner", shared_test_tool_spec("cancel_tool"));

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-cancel-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), cid);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancel-call".into(),
            name: ToolName::new("cancel_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("tool call routed");

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
        agent_prompt_id: None,
    });

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id.as_str() == "cancel-call"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancel-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Regression for tau-agent-95m: the harness host API used by the built-in
/// `cancel` tool must only accept target calls owned by the requesting
/// conversation. Otherwise one agent that learns another agent's call id can
/// broadcast a cancellation request for unrelated work.
#[test]
fn cancel_request_api_rejects_non_owner_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let owner = AgentId::parse("owner").expect("owner id");
    let attacker = AgentId::parse("attacker").expect("attacker id");
    let target: ToolCallId = "owned-running-call".into();

    h.tool_agents.insert(target.clone(), owner.clone());
    h.pending_tools.insert(
        target.clone(),
        PendingTool {
            name: ToolName::new("slow_tool"),
            internal_name: ToolName::new("slow_tool"),
            tool_type: tau_proto::ToolType::Function,
        },
    );

    assert!(!h.is_running_cancellable_tool_call_for(&attacker, &target));
    assert_eq!(
        h.publish_tool_cancel_request_for(&attacker, target.clone())
            .expect_err("non-owner must not cancel the call"),
        "Unknown tool call id"
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == target
    )));

    assert!(h.is_running_cancellable_tool_call_for(&owner, &target));
    h.publish_tool_cancel_request_for(&owner, target.clone())
        .expect("owner may cancel the call");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == target
    )));

    h.shutdown().expect("shutdown");
}

/// Completed-call diagnostics are also conversation-scoped so a caller cannot
/// probe whether another agent's guessed call id existed earlier in the
/// session.
#[test]
fn completed_tool_call_lookup_is_owner_scoped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let owner = AgentId::parse("owner").expect("owner id");
    let attacker = AgentId::parse("attacker").expect("attacker id");
    let target: ToolCallId = "owned-completed-call".into();

    h.tool_agents.insert(target.clone(), owner.clone());
    h.pending_tools.insert(
        target.clone(),
        PendingTool {
            name: ToolName::new("slow_tool"),
            internal_name: ToolName::new("slow_tool"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.clear_tool_call_tracking(target.as_str());

    assert!(h.is_completed_tool_call_for(&owner, &target));
    assert!(!h.is_completed_tool_call_for(&attacker, &target));

    h.shutdown().expect("shutdown");
}

/// Regression: harness-authored turn cancellation must not publish a second
/// transcript-terminal `ToolCancelled` after a background placeholder has
/// already closed the foreground tool round. Backgrounded calls are completed
/// through the durable background channel instead.
#[test]
fn cancel_remaining_backgrounded_extension_call_publishes_background_error_only() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-cancel-bg");
    h.registry.register(
        "conn-cancel-bg",
        instant_background_test_tool_spec("cancel_bg_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-cancel-bg-tool".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "cancel-bg-call".into(),
            name: ToolName::new("cancel_bg_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("tool call routed");

    let call_id: ToolCallId = "cancel-bg-call".into();
    assert_eq!(background_placeholder_count(&h, call_id.as_str()), 1);
    assert!(h.tool_turn.is_backgrounded(&call_id));

    h.cancel_remaining_tool_calls(
        &cid,
        vec![call_id.clone()],
        "test cancel",
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelRequest(request) if request.target_call_id == call_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id == call_id && error.message == "Tool call canceled"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice) if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
    )));
    assert!(!h.tool_turn.is_backgrounded(&call_id));
    assert!(!h.pending_tool_providers.contains_key(&call_id));
    assert!(!h.tool_agents.contains_key(&call_id));
    assert!(
        h.agents[&cid].pending_prompts.iter().any(|prompt| {
            prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
        }),
        "live-branch cancellation should leave a queued internal completion notice"
    );

    h.shutdown().expect("shutdown");
}

/// Regression: live user cancellation of a turn with an already-backgrounded
/// call must keep a queued internal completion notice on the live branch. The
/// notice should not auto-advance immediately, otherwise canceling a turn could
/// cause the model to restart solely to observe harness-authored cancellation.
#[test]
fn live_cancel_backgrounded_tool_queues_completion_notice_without_advancing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    let call_id: ToolCallId = "live-cancel-bg-call".into();
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("live_cancel_bg_tool"),
            internal_name: ToolName::new("live_cancel_bg_tool"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.tool_turn
        .record_in_flight_for_test(cid.clone(), call_id.clone());
    assert!(h.tool_turn.mark_backgrounded(&call_id));
    h.publish_synthetic_background_result(&call_id);
    seed_tools_running(&mut h, &cid, vec![call_id.clone()]);
    h.agents
        .get_mut(&cid)
        .expect("conversation")
        .pending_prompts
        .push_back(PendingPrompt::user(
            "queued user prompt to discard".to_owned(),
        ));

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
        agent_prompt_id: None,
    });

    let completion_prompt = background_completion_prompt(&call_id);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ToolBackgroundError(error) if error.call_id == call_id
        )),
        1
    );
    assert!(matches!(h.agents[&cid].turn_state, AgentTurnState::Idle));
    assert!(h.agents[&cid].pending_cancel.is_none());
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == completion_prompt
                && prompt.is_passive_background_completion()),
        "live branch should retain the internal background completion notice"
    );
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .all(PendingPrompt::is_passive_background_completion),
        "live cancel should still discard stale non-internal queued prompts"
    );
    h.try_advance_queue();
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == completion_prompt
    )));
    assert!(matches!(h.agents[&cid].turn_state, AgentTurnState::Idle));

    let submission = h
        .submit_user_prompt("s1".into(), "continue after cancel".to_owned())
        .expect("submit follow-up user prompt");
    assert_eq!(submission, PromptSubmission::Dispatched);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == completion_prompt
                && submitted.message_class == tau_proto::PromptMessageClass::Internal
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == "continue after cancel"
                && submitted.message_class == tau_proto::PromptMessageClass::User
    )));
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt),
        "follow-up user prompt should consume the passive background notice"
    );
    assert!(!h.tool_turn.is_backgrounded(&call_id));
    assert!(!h.pending_tools.contains_key(&call_id));
    assert!(!h.tool_agents.contains_key(&call_id));

    h.shutdown().expect("shutdown");
}

/// Regression: a passive background notice left on the canceled live branch
/// must not block ordinary queued work for other agents. Once passive-only
/// queues are ignored by `next_runnable_agent`, live cancellation should still
/// advance the global queue so another agent's normal prompt can run.
#[test]
fn live_cancel_passive_notice_still_advances_other_runnable_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cancel_cid = ensure_test_user_agent(&mut h);
    let cancel_agent_id = h.agents[&cancel_cid].agent_id.clone().expect("agent id");
    let other_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    h.agents
        .get_mut(&other_cid)
        .expect("other conversation")
        .pending_prompts
        .push_back(PendingPrompt::user("other agent prompt".to_owned()));

    let call_id: ToolCallId = "live-cancel-bg-with-other-agent".into();
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("live_cancel_bg_tool"),
            internal_name: ToolName::new("live_cancel_bg_tool"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.tool_agents.insert(call_id.clone(), cancel_cid.clone());
    h.tool_turn
        .record_in_flight_for_test(cancel_cid.clone(), call_id.clone());
    assert!(h.tool_turn.mark_backgrounded(&call_id));
    h.publish_synthetic_background_result(&call_id);
    seed_tools_running(&mut h, &cancel_cid, vec![call_id.clone()]);

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&cancel_agent_id)),
        agent_prompt_id: None,
    });

    assert!(
        h.agents[&cancel_cid].pending_prompts.iter().any(|prompt| {
            prompt.text == background_completion_prompt(&call_id)
                && prompt.is_passive_background_completion()
        }),
        "canceled agent should keep only a passive completion notice"
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == background_completion_prompt(&call_id)
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "other agent prompt"
    )));
    assert!(matches!(
        h.agents[&other_cid].turn_state,
        AgentTurnState::AgentThinking { .. }
    ));

    h.shutdown().expect("shutdown");
}

/// Regression: live cancellation must include backgrounded calls that have
/// already left `ToolsRunning.remaining_calls` while a sibling foreground tool
/// keeps the same turn active. Otherwise the background placeholder would stay
/// unresolved until the background task eventually reported back.
#[test]
fn live_cancel_tools_running_includes_already_backgrounded_siblings() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-live-mixed-cancel");
    h.registry.register(
        "conn-live-mixed-cancel",
        instant_background_test_tool_spec("live_bg_tool"),
    );
    h.registry.register(
        "conn-live-mixed-cancel",
        shared_test_tool_spec("live_foreground_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-live-mixed-cancel".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    let bg_call_id: ToolCallId = "live-bg-sibling".into();
    let fg_call_id: ToolCallId = "live-fg-sibling".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: bg_call_id.clone(),
                name: ToolName::new("live_bg_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: fg_call_id.clone(),
                name: ToolName::new("live_foreground_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("mixed tool turn starts");

    assert!(h.tool_turn.is_backgrounded(&bg_call_id));
    assert!(matches!(
        h.agents[&cid].turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(h.agents[&cid].pending_prompts.is_empty());

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
        agent_prompt_id: None,
    });

    assert_eq!(background_error_count(&h, bg_call_id.as_str()), 1);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == fg_call_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == bg_call_id
    )));
    assert!(
        h.agents[&cid].pending_prompts.iter().any(|prompt| {
            prompt.text == background_completion_prompt(&bg_call_id)
                && prompt.is_passive_background_completion()
        }),
        "backgrounded sibling should receive a passive completion notice"
    );
    assert!(!h.tool_turn.is_backgrounded(&bg_call_id));
    assert!(!h.pending_tools.contains_key(&bg_call_id));
    assert!(!h.pending_tools.contains_key(&fg_call_id));

    h.shutdown().expect("shutdown");
}

struct TestAgentStartBuiltin;

impl crate::InternalToolHandler for TestAgentStartBuiltin {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![tau_proto::ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: Some("test agent_start".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({"type":"object"})),
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Never),
            examples: Vec::new(),
        }]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == "agent_start"
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::ToolStarted(started) => {
                let Some((_conversation_id, call, _visible_tool_name)) =
                    host.internal_started_call(started)
                else {
                    return Ok(());
                };
                let query_id = format!("test-agent-start-{}", call.id);
                host.enqueue_start_agent_request_without_draining(tau_proto::StartAgentRequest {
                    parent_agent: None,
                    query_id,
                    instruction: "test builtin agent_start".to_owned(),
                    role: None,
                    input_stats: tau_proto::ToolUseStats::default(),
                    tool_call_id: Some(call.id.clone()),
                    task_name: Some("test agent_start".to_owned()),
                })
                .map_err(HarnessError::Participant)?;
                host.background_tool_call(
                    &call.id,
                    CborValue::Text("test builtin agent_start running in background".to_owned()),
                );
                host.drain_start_agent_requests()
            }
            Event::ToolCancelRequest(request) => {
                let query_id = format!("test-agent-start-{}", request.target_call_id);
                let _ = host.cancel_start_agent_request(&query_id, &request.target_call_id, true);
                Ok(())
            }
            Event::StartAgentResult(result) => {
                let Some(call_id) = result.query_id.strip_prefix("test-agent-start-") else {
                    return Ok(());
                };
                host.finish_prebuilt_tool_error(tau_proto::ToolError {
                    call_id: call_id.into(),
                    tool_name: ToolName::new("agent_start"),
                    tool_type: tau_proto::ToolType::Function,
                    message: result
                        .error
                        .clone()
                        .unwrap_or_else(|| "test agent_start failed".to_owned()),
                    details: None,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Regression: a harness-owned, backgrounded `agent_start` must be completed
/// through the background-error channel when delegate teardown cancels it. A
/// second transcript-terminal `ToolCancelled` would make the store reject the
/// already-closed background placeholder branch.
#[test]
fn cancel_backgrounded_builtin_agent_start_publishes_background_error_only() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);

    let parent_cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-builtin-agent-start-bg".into();
    seed_agent_thinking(&mut h, &parent_cid, spid.as_str());
    let parent_agent_id = h.agents[&parent_cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), parent_cid.clone());
    let call_id: ToolCallId = "builtin-agent-start-bg".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("builtin agent_start dispatched");
    assert_eq!(background_placeholder_count(&h, call_id.as_str()), 1);
    assert!(h.tool_turn.is_backgrounded(&call_id));

    let query_id = format!("test-agent-start-{call_id}");
    let side_cid = ext_query_cid(&h, &query_id).expect("side conversation");
    assert_eq!(
        h.agents
            .get(&side_cid)
            .and_then(|agent| agent.parent_tool_call_id.as_ref()),
        Some(&call_id)
    );

    h.cancel_remaining_tool_calls(
        &parent_cid,
        vec![call_id.clone()],
        "harness teardown",
        BackgroundCompletionPromptMode::DoNotQueue,
    );

    assert_eq!(background_error_count(&h, call_id.as_str()), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
        )),
        0
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice) if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
    )));
    assert!(!h.tool_turn.is_backgrounded(&call_id));
    assert!(!h.pending_tools.contains_key(&call_id));
    assert!(!h.tool_agents.contains_key(&call_id));

    h.shutdown().expect("shutdown");
}

/// Regression: live turn cancellation of backgrounded builtin `agent_start`
/// must still leave a passive completion notice even though the builtin cancel
/// handler suppresses normal background prompts while synchronously resolving
/// the `StartAgentResult`.
#[test]
fn live_cancel_backgrounded_builtin_agent_start_keeps_passive_completion_notice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);

    let parent_cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-live-builtin-agent-start-bg".into();
    seed_agent_thinking(&mut h, &parent_cid, spid.as_str());
    let parent_agent_id = h.agents[&parent_cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), parent_cid.clone());
    let call_id: ToolCallId = "live-builtin-agent-start-bg".into();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("builtin agent_start dispatched");
    assert!(h.tool_turn.is_backgrounded(&call_id));

    seed_tools_running(&mut h, &parent_cid, vec![call_id.clone()]);
    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&parent_agent_id)),
        agent_prompt_id: None,
    });

    assert_eq!(background_error_count(&h, call_id.as_str()), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(
        h.agents[&parent_cid].pending_prompts.iter().any(|prompt| {
            prompt.text == background_completion_prompt(&call_id)
                && prompt.is_passive_background_completion()
        }),
        "live builtin cancellation should not lose the passive completion notice"
    );
    assert!(
        !h.suppressed_background_completion_prompts
            .contains(&call_id)
    );
    assert!(!h.tool_turn.is_backgrounded(&call_id));
    assert!(!h.pending_tools.contains_key(&call_id));
    assert!(!h.tool_agents.contains_key(&call_id));

    h.shutdown().expect("shutdown");
}

/// Regression: cancellation routing must re-check background state after the
/// cancel request has been published. A future synchronous cancel-request
/// handler may turn a still-foreground call into a backgrounded call without
/// completing it; that must be finalized through `ToolBackgroundError`, not a
/// second foreground `ToolCancelled`.
#[test]
fn cancel_target_rechecks_background_state_after_cancel_request() {
    let (_td, mut h) = setup_routed_test_tool_call("post-request-bg-call", "post_request_bg_tool");
    let call_id: ToolCallId = "post-request-bg-call".into();
    let target = super::super::CancelTarget {
        call_id: call_id.clone(),
        tool_name: ToolName::new("post_request_bg_tool"),
        tool_type: tau_proto::ToolType::Function,
        backgrounded: false,
    };

    assert!(!h.cancel_target_should_finish_as_background_error(&target));
    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
            target_call_id: call_id.clone(),
        }),
    );
    assert!(h.tool_turn.mark_backgrounded(&call_id));
    h.publish_synthetic_background_result(&call_id);

    assert!(h.cancel_target_should_finish_as_background_error(&target));
    h.finish_backgrounded_tool_cancelled_by_harness(
        target,
        BackgroundCompletionPromptMode::QueuePassive,
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id == call_id
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id == call_id && error.message == "Tool call canceled"
    )));
    assert!(!h.tool_turn.is_backgrounded(&call_id));
    assert!(!h.tool_agents.contains_key(&call_id));

    h.shutdown().expect("shutdown");
}

/// Cancelling a turn while `wait` is blocked must remove the waiter entry. A
/// later wait for the same target should report the cancelled/consumed target,
/// not a stale "existing wait" from the aborted wait call.
#[test]
fn cancel_clears_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let target_call_id: ToolCallId = "wait-target".into();
    let wait_call_id: ToolCallId = "wait-call".into();

    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    h.tool_agents.insert(target_call_id.clone(), cid.clone());
    h.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    seed_tools_running(
        &mut h,
        &cid,
        vec![target_call_id.clone(), wait_call_id.clone()],
    );

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
        agent_prompt_id: None,
    });

    let second_wait_call = AgentToolCall {
        id: "wait-call-2".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &second_wait_call, ToolName::new("wait"))
        .expect("second wait");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call-2"
                && error.message.contains("already consumed")
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn cancel_while_thinking_terminates_prompt_and_drops_late_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-cancel-thinking".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.agents
        .get_mut(&cid)
        .expect("conversation")
        .in_flight_prompt = Some(spid.clone());
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("agent id");
    h.prompt_agents.insert(spid.clone(), cid.clone());

    h.handle_cancel_prompt(&tau_proto::UiCancelPrompt {
        session_id: "s1".into(),
        target_agent_id: Some(crate::parse_agent_id(&target_agent_id)),
        agent_prompt_id: None,
    });

    assert!(matches!(h.agents[&cid].turn_state, AgentTurnState::Idle));
    assert!(h.agents[&cid].in_flight_prompt.is_none());
    assert!(h.agents[&cid].pending_cancel.is_none());
    assert!(h.canceled_prompts.contains(&spid));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptTerminated(terminated)
            if terminated.agent_prompt_id == spid
                && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
    )));
    let response_count_before = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid.clone(),
        agent_id: crate::parse_agent_id(&target_agent_id),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "(cancelled by harness)".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
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
    })
    .expect("late canceled response should be ignored");

    let response_count_after = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();
    assert_eq!(response_count_after, response_count_before);
    assert!(!h.canceled_prompts.contains(&spid));

    h.shutdown().expect("shutdown");
}

#[test]
fn cross_session_submission_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.selected_model = Some("test/model".into());
    let submission = h
        .submit_user_prompt("chat-1".into(), "hello".to_owned())
        .expect("submit");
    match submission {
        PromptSubmission::Rejected { reason } => {
            assert!(reason.contains("s1"), "reason should name bound session");
            assert!(reason.contains("chat-1"), "reason should name rejected id");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(
        h.agents
            .values()
            .all(|agent| agent.pending_prompts.is_empty()),
        "rejected prompt must not queue"
    );
    assert!(
        h.store.session("chat-1").is_none(),
        "rejected session must not be created"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_model_prompt_routes_directly_to_provider_owner() {
    // Provider-published models should not wake every provider subscriber.
    // The committed prompt remains visible to observers, while the owner gets a
    // direct delivery even without subscribing to agent.prompt_created.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let provider_frames =
        connect_test_client(&mut h, "provider-owner", tau_proto::ClientKind::Provider);
    let provider_observer_frames =
        connect_test_client(&mut h, "provider-observer", tau_proto::ClientKind::Provider);
    let ui_frames = connect_test_client(&mut h, "ui-observer", tau_proto::ClientKind::Ui);
    let prompt_selector = vec![EventSelector::Exact(
        tau_proto::EventName::AGENT_PROMPT_CREATED,
    )];
    h.bus
        .set_subscriptions("provider-observer", Vec::new(), prompt_selector.clone())
        .expect("provider observer subscription");
    h.bus
        .set_subscriptions("ui-observer", Vec::new(), prompt_selector)
        .expect("ui observer subscription");

    h.handle_extension_message(
        "provider-owner",
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("provider ready");
    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    tags: Vec::new(),
                    default_affinity: 0,
                    context_window: 200_000,
                    efforts: vec![tau_proto::Effort::Medium],
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    h.provider_model_info.insert(
        model_id.clone(),
        tau_proto::ProviderModelInfo {
            id: model_id.clone(),
            display_name: None,
            tags: Vec::new(),
            default_affinity: 0,
            context_window: 200_000,
            efforts: vec![tau_proto::Effort::Medium],
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: false,
        },
    );
    h.provider_model_routes
        .insert(model_id.clone(), "provider-owner".into());
    h.available_roles
        .get_mut(&h.selected_role)
        .expect("selected role")
        .model = Some(model_id.clone());
    h.selected_model = Some(model_id);

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid = h.send_prompt_to_agent("s1");

    let frame_is_prompt = |routed: &RoutedFrame, spid: &AgentPromptId| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::AgentPromptCreated(prompt))
                if prompt.agent_prompt_id.as_str() == spid.as_str()
        )
    };
    assert!(
        provider_frames
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "provider owner should receive the direct prompt request"
    );
    assert!(
        ui_frames
            .lock()
            .expect("ui frames")
            .iter()
            .any(|routed| frame_is_prompt(routed, &spid)),
        "UI observer should still see the committed prompt fact"
    );
    assert!(
        provider_observer_frames
            .lock()
            .expect("provider observer frames")
            .is_empty(),
        "provider observers should not receive provider-owned prompt execution"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_execution_events_must_come_from_prompt_owner() {
    // Provider execution is point-to-point. Once the harness routes a prompt to
    // the provider that published the selected model, streaming and final
    // response events for that prompt must come back from the same connection.
    // Otherwise a second provider participant could spoof a response for an
    // in-flight prompt it never received.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let _owner_frames =
        connect_test_client(&mut h, "provider-owner", tau_proto::ClientKind::Provider);
    let _other_frames =
        connect_test_client(&mut h, "provider-other", tau_proto::ClientKind::Provider);
    let _tool_frames =
        connect_test_client(&mut h, "tool-impersonator", tau_proto::ClientKind::Tool);

    h.handle_extension_message(
        "provider-owner",
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("provider ready");
    let model_id: tau_proto::ModelId = "openai/gpt-5.5".parse().expect("model id");
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![tau_proto::ProviderModelInfo {
                    id: model_id.clone(),
                    display_name: None,
                    tags: Vec::new(),
                    default_affinity: 0,
                    context_window: 200_000,
                    efforts: vec![tau_proto::Effort::Medium],
                    verbosities: vec![tau_proto::Verbosity::Medium],
                    thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                    supports_compaction: false,
                }],
            },
        )),
    )
    .expect("provider model snapshot");
    h.provider_model_info.insert(
        model_id.clone(),
        tau_proto::ProviderModelInfo {
            id: model_id.clone(),
            display_name: None,
            tags: Vec::new(),
            default_affinity: 0,
            context_window: 200_000,
            efforts: vec![tau_proto::Effort::Medium],
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: false,
        },
    );
    h.provider_model_routes
        .insert(model_id.clone(), "provider-owner".into());
    h.available_roles
        .get_mut(&h.selected_role)
        .expect("selected role")
        .model = Some(model_id.clone());
    h.selected_model = Some(model_id);

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid = h.send_prompt_to_agent("s1");
    assert_eq!(
        h.pending_provider_prompts.get(&spid).map(|id| id.as_str()),
        Some("provider-owner"),
        "outbound prompt owner should be recorded"
    );

    h.handle_extension_event(
        "provider-other",
        TestProtocolItem::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            agent_id: durable_agent_id_for_conversation(&h, &test_user_agent(&h)),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("forged stream from provider");
    h.handle_extension_event(
        "tool-impersonator",
        TestProtocolItem::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            agent_id: durable_agent_id_for_conversation(&h, &test_user_agent(&h)),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("forged stream from tool");
    h.handle_extension_event(
        "provider-other",
        TestProtocolItem::Event(Event::ProviderResponseFinished(provider_text_response(
            &spid,
            durable_agent_id_for_conversation(&h, &test_user_agent(&h)),
            "spoofed final",
        ))),
    )
    .expect("forged final response");

    assert_eq!(
        h.pending_provider_prompts.get(&spid).map(|id| id.as_str()),
        Some("provider-owner"),
        "wrong-source events must not consume the pending owner"
    );
    assert!(matches!(
        h.agents[&test_user_agent(&h)].turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert!(!event_log_contains(&h, "provider-other", |event| matches!(
        event,
        Event::ProviderResponseUpdated(_) | Event::ProviderResponseFinished(_)
    )));
    assert!(!event_log_contains(
        &h,
        "tool-impersonator",
        |event| matches!(
            event,
            Event::ProviderResponseUpdated(_) | Event::ProviderResponseFinished(_)
        )
    ));

    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            agent_id: durable_agent_id_for_conversation(&h, &test_user_agent(&h)),
            deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "real".to_owned(),
                phase: None,
            }],
            compaction: None,
            status: None,
            response_stats: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("owner stream");
    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseFinished(provider_text_response(
            &spid,
            durable_agent_id_for_conversation(&h, &test_user_agent(&h)),
            "real final",
        ))),
    )
    .expect("owner final response");

    assert!(!h.pending_provider_prompts.contains_key(&spid));
    assert!(matches!(
        h.agents[&test_user_agent(&h)].turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::ProviderResponseUpdated(_)
        )),
        0,
        "provider response updates must remain transient"
    );
    assert!(event_log_contains(&h, "provider-owner", |event| matches!(
        event,
        Event::ProviderResponseFinished(_)
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures provider-owned response stats survive harness validation and are
/// broadcast on `provider.response_updated` without a harness stats projection.
#[test]
fn provider_response_stats_are_public_provider_updates() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    connect_test_client(&mut h, "provider-owner", tau_proto::ClientKind::Provider);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-provider-public-stats".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    let durable_id = durable_agent_id_for_conversation(&h, &cid);
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.pending_provider_prompts
        .insert(spid.clone(), "provider-owner".into());
    let ui_frames = connect_test_client(&mut h, "ui-stats-observer", tau_proto::ClientKind::Ui);
    h.bus
        .set_subscriptions(
            "ui-stats-observer",
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::PROVIDER_RESPONSE_UPDATED),
                EventSelector::Exact(tau_proto::EventName::AGENT_STATS_UPDATED),
            ],
        )
        .expect("ui stats subscriptions");

    h.handle_extension_event(
        "provider-owner",
        TestProtocolItem::Event(Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            agent_id: crate::parse_agent_id("forged_agent"),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: Some(tau_proto::ProviderResponseStats {
                current: tau_proto::ProviderResponseStatsSample {
                    response_bytes_received: 4096,
                    elapsed_micros: 2_000_000,
                },
                previous: tau_proto::ProviderResponseStatsSample {
                    response_bytes_received: 1024,
                    elapsed_micros: 1_000_000,
                },
            }),
            originator: tau_proto::PromptOriginator::User,
        })),
    )
    .expect("provider stats update");

    let frames = ui_frames.lock().expect("ui frames");
    assert!(
        frames.iter().any(|routed| matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ProviderResponseUpdated(update))
                if update.agent_id == durable_id
                    && update.response_stats.as_ref().is_some_and(|stats|
                        stats.current.response_bytes_received == 4096
                            && stats.previous.response_bytes_received == 1024)
        )),
        "provider stats update must broadcast unchanged as provider.response_updated"
    );
    assert!(
        frames.iter().all(|routed| !matches!(
            peel_inner_event(&routed.frame),
            Some(Event::AgentStatsUpdated(_))
        )),
        "provider response stats must not be projected into harness agent stats"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_turn_dispatches_provider_calls_without_global_locking() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid);

    // A `read` of a nonexistent path returns a ToolError; a valid mutating
    // tool call returns ToolResult. Harness dispatch no longer serializes them
    // by execution mode; ext-shell owns update coordination.
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text("/nonexistent/tau-test-path".to_owned()),
    )]);
    let edit_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("w.txt").display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("start_line".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("end_line_exclusive".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("newText".to_owned()),
                    CborValue::Text("hi".to_owned()),
                ),
                (
                    CborValue::Text("context_line".to_owned()),
                    CborValue::Text(String::new()),
                ),
            ])]),
        ),
    ]);
    let response = ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    h.handle_provider_response_finished(response)
        .expect("finished");

    for call_id in ["c1", "c2", "c3"] {
        assert!(
            h.tool_turn.is_in_flight(&ToolCallId::from(call_id)),
            "{call_id} should dispatch immediately"
        );
    }
    assert_eq!(h.tool_turn.pending_len(), 0);
    assert_eq!(h.tool_turn.in_flight_len(), 3);

    drive_harness_until_tool_turn_empty(&mut h);
    assert!(h.tool_turn.is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn multi_tool_turn_keeps_all_results_in_followup_prompt() {
    // Regression: when several tool calls complete in sequence, every
    // ToolResult must end up on the current branch so the follow-up
    // prompt sees a balanced tool_use ↔ tool_result set. A previous
    // bug let `publish_event` (used by the ToolResult/ToolError path)
    // leave the conversation's local head stale, so the next
    // ToolRequest's `publish_for_agent` emitted a
    // `UiNavigateTree` that bounced the tree head backward — orphaning
    // the just-published ToolResult onto a dead branch and triggering
    // OpenAI's "No tool output found for function call ..." 400.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "go");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = format!("ap-{agent_id}-0").into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid);

    let edit_args = |name: &str| {
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join(name).display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("start_line".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("end_line_exclusive".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text(name.to_owned()),
                    ),
                    (
                        CborValue::Text("context_line".to_owned()),
                        CborValue::Text(String::new()),
                    ),
                ])]),
            ),
        ])
    };
    let response = ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("a.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("b.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("c.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(response)
        .expect("finished");

    drive_harness_until_tool_turn_empty(&mut h);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));

    // After all three tools complete, the harness has auto-dispatched
    // a follow-up prompt. Read its context items and check that every
    // tool call has a matching tool result on the same branch.
    let prompt = read_nth_prompt_created(&h, 0);
    let tool_use_ids: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_result_ids: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert_eq!(
        tool_use_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "follow-up prompt must keep every tool_use; got {tool_use_ids:?}"
    );
    assert_eq!(
        tool_result_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "every tool_use must be paired with a tool_result on the current branch; \
         got {tool_result_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn ui_navigate_tree_can_reselect_agent_head_after_resume() {
    // Branch selection is UI-owned across process restarts. The harness should
    // honor the durable agent id when the UI replays its selected node after a
    // resume, so the next user message branches from that per-agent head.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let first_user_head;
    let agent_id: tau_proto::AgentId;

    {
        let mut h = echo_harness(&sp).expect("start");

        append_user_message_via_event(&mut h, "s1", "first branch point");
        let cid = ensure_test_user_agent(&mut h);
        first_user_head = h.agents[&cid].head.expect("first user head");
        append_user_message_via_event(&mut h, "s1", "second branch point");
        agent_id = crate::parse_agent_id(
            h.agents[&cid]
                .agent_id
                .clone()
                .expect("default conversation agent id"),
        );

        h.handle_ui_navigate_tree(
            "ui",
            tau_proto::UiNavigateTree {
                session_id: "s1".into(),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Node(first_user_head),
            },
        )
        .expect("navigate tree");

        assert_eq!(h.agents[&cid].head, Some(first_user_head));
        assert!(loaded_agent_events(&h, "s1").into_iter().any(|event| {
            matches!(
                event,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: ref moved_agent_id,
                    head: tau_proto::AgentHead::Node(node_id),
                }) if moved_agent_id == &agent_id && node_id == first_user_head
            )
        }));

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);

        h.handle_ui_navigate_tree(
            "ui",
            tau_proto::UiNavigateTree {
                session_id: "s1".into(),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Node(first_user_head),
            },
        )
        .expect("reselect tree head after resume");

        assert_eq!(h.agents[&cid].head, Some(first_user_head));

        append_user_message_via_event(&mut h, "s1", "branched after resume");
        let branched = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("branched node after resume");
        assert_eq!(branched.parent_id, Some(first_user_head));

        h.shutdown().expect("shutdown");
    }
}

#[test]
fn ui_tree_prompt_anchor_rewinds_before_prompt_not_to_raw_node() {
    // Default `/tree <anchor>` navigation uses prompt rewind anchors. A
    // numeric anchor matching an assistant node id must not silently select
    // that raw node; raw node selection requires the explicit `node` target.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    append_user_message_via_event(&mut h, "s1", "first prompt");
    let first_prompt_node = h.agents[&cid].head.expect("first prompt node");
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &"sp-tree-anchor".into(),
            agent_id.clone(),
            "assistant answer",
        )),
    );
    let assistant_node = h.agents[&cid].head.expect("assistant node");
    assert_ne!(first_prompt_node, assistant_node);

    h.handle_ui_navigate_tree(
        "ui",
        tau_proto::UiNavigateTree {
            session_id: "s1".into(),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(1),
        },
    )
    .expect("navigate to first prompt anchor");
    assert_eq!(
        h.agents[&cid].head, None,
        "anchor 1 rewinds before the first prompt instead of selecting raw node 1"
    );

    h.handle_ui_navigate_tree(
        "ui",
        tau_proto::UiNavigateTree {
            session_id: "s1".into(),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::Node(assistant_node),
        },
    )
    .expect("navigate to raw assistant node explicitly");
    assert_eq!(h.agents[&cid].head, Some(assistant_node));
}

#[test]
fn ui_tree_prompt_anchor_rewinds_before_later_prompt() {
    // Selecting a later prompt anchor should move the branch head to that
    // prompt node's parent, so the next prompt replaces/branches before the
    // selected user prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    append_user_message_via_event(&mut h, "s1", "first prompt");
    h.publish_for_agent(
        &cid,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: "synthetic injected input should not be listed".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentCompactionTriggered(tau_proto::AgentCompactionTriggered {
            agent_id: agent_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id.clone(),
            text: "internal prompt should not be listed".to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &"sp-tree-anchor-parent".into(),
            agent_id.clone(),
            "assistant answer",
        )),
    );
    let assistant_node = h.agents[&cid].head.expect("assistant node");
    append_user_message_via_event(&mut h, "s1", "second prompt");

    h.handle_ui_navigate_tree(
        "ui",
        tau_proto::UiNavigateTree {
            session_id: "s1".into(),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(2),
        },
    )
    .expect("navigate before second prompt");
    assert_eq!(h.agents[&cid].head, Some(assistant_node));

    append_user_message_via_event(&mut h, "s1", "replacement second prompt");
    let branched = default_agent_tree(&h)
        .nodes()
        .last()
        .expect("branched prompt");
    assert_eq!(branched.parent_id, Some(assistant_node));
}

#[test]
fn ui_tree_root_navigation_persists_across_resume() {
    // Root navigation must be a durable head state, not just a runtime `None`,
    // so resuming the session keeps the next prompt branching before the first
    // prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id: tau_proto::AgentId;

    {
        let mut h = quiet_provider_harness(&sp).expect("start");
        append_user_message_via_event(&mut h, "s1", "first prompt");
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);

        h.handle_ui_navigate_tree(
            "ui",
            tau_proto::UiNavigateTree {
                session_id: "s1".into(),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Root,
            },
        )
        .expect("navigate to root");
        assert_eq!(h.agents[&cid].head, None);
        assert!(loaded_agent_events(&h, "s1").into_iter().any(|event| {
            matches!(
                event,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: ref moved_agent_id,
                    head: tau_proto::AgentHead::Root,
                }) if moved_agent_id == &agent_id
            )
        }));

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        assert_eq!(h.agents[&cid].head, None);

        append_user_message_via_event(&mut h, "s1", "root branch after resume");
        let branched = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("branched prompt after resume");
        assert_eq!(branched.parent_id, None);

        h.shutdown().expect("shutdown");
    }
}

#[test]
fn resume_keeps_prompt_appended_after_root_rewind_as_head() {
    // A durable root head move is only the cursor until a later transcript node
    // is appended. Resume must restore the replayed final tree head, not the
    // stale root rewind event.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let replacement_node: tau_core::NodeId;

    {
        let mut h = quiet_provider_harness(&sp).expect("start");
        append_user_message_via_event(&mut h, "s1", "first prompt");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);

        h.handle_ui_navigate_tree(
            "ui",
            tau_proto::UiNavigateTree {
                session_id: "s1".into(),
                target_agent_id: Some(agent_id),
                target: tau_proto::UiTreeNavigationTarget::Root,
            },
        )
        .expect("navigate to root");
        append_user_message_via_event(&mut h, "s1", "replacement root prompt");
        replacement_node = h.agents[&cid].head.expect("replacement node");

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        assert_eq!(h.agents[&cid].head, Some(replacement_node));

        append_user_message_via_event(&mut h, "s1", "continues after replacement");
        let continued = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("continued prompt after resume");
        assert_eq!(continued.parent_id, Some(replacement_node));

        h.shutdown().expect("shutdown");
    }
}

#[test]
fn resume_keeps_prompt_appended_after_anchor_rewind_as_head() {
    // A prompt-anchor rewind moves the cursor to the selected prompt's parent,
    // but a later replacement prompt must become the restored head after resume.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let replacement_node: tau_core::NodeId;

    {
        let mut h = quiet_provider_harness(&sp).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);

        append_user_message_via_event(&mut h, "s1", "first prompt");
        h.publish_for_agent(
            &cid,
            Event::ProviderResponseFinished(provider_text_response(
                &"sp-tree-resume-anchor".into(),
                agent_id.clone(),
                "assistant answer",
            )),
        );
        append_user_message_via_event(&mut h, "s1", "second prompt");

        h.handle_ui_navigate_tree(
            "ui",
            tau_proto::UiNavigateTree {
                session_id: "s1".into(),
                target_agent_id: Some(agent_id),
                target: tau_proto::UiTreeNavigationTarget::PromptAnchor(2),
            },
        )
        .expect("navigate before second prompt");
        append_user_message_via_event(&mut h, "s1", "replacement second prompt");
        replacement_node = h.agents[&cid].head.expect("replacement node");

        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let cid = ensure_test_user_agent(&mut h);
        assert_eq!(h.agents[&cid].head, Some(replacement_node));

        append_user_message_via_event(&mut h, "s1", "continues after replacement");
        let continued = default_agent_tree(&h)
            .nodes()
            .last()
            .expect("continued prompt after resume");
        assert_eq!(continued.parent_id, Some(replacement_node));

        h.shutdown().expect("shutdown");
    }
}

#[test]
fn ui_tree_request_lists_prompt_anchors_without_raw_nodes() {
    // The default tree view is the user-facing rewind surface: it lists root
    // plus prompt anchors and does not expose assistant/tool raw node ids.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    append_user_message_via_event(&mut h, "s1", "first prompt");
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &"sp-tree-view".into(),
            agent_id.clone(),
            "assistant answer should not be listed",
        )),
    );
    append_user_message_via_event(&mut h, "s1", "second prompt");

    h.handle_tree_request(&"s1".into(), Some(agent_id.as_str()));
    let notice_messages: Vec<String> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(notice) => Some(notice.message),
            _ => None,
        })
        .collect();

    assert!(
        notice_messages
            .iter()
            .any(|message| message.contains("0") && message.contains("before first prompt")),
        "root anchor should be listed: {notice_messages:?}"
    );
    assert!(
        notice_messages
            .iter()
            .any(|message| message.contains("1") && message.contains("first prompt")),
        "first prompt anchor should be listed: {notice_messages:?}"
    );
    assert!(
        notice_messages
            .iter()
            .any(|message| message.contains("2") && message.contains("second prompt")),
        "second prompt anchor should be listed without synthetic inputs shifting numbering: {notice_messages:?}"
    );
    assert!(
        !notice_messages
            .iter()
            .any(|message| message.contains("assistant answer should not be listed")),
        "assistant raw node should not be part of default /tree output: {notice_messages:?}"
    );
    assert!(
        !notice_messages.iter().any(|message| message
            .contains("synthetic injected input should not be listed")
            || message.contains("internal prompt should not be listed")),
        "synthetic/internal inputs should not be part of default /tree output: {notice_messages:?}"
    );
}

#[test]
fn queued_prompt_is_steered_into_next_round_after_tool_result() {
    // While the agent is mid-turn (a tool is in flight), a fresh user
    // prompt must queue rather than dispatch. When the tool result
    // arrives and the harness is about to issue the next-round prompt,
    // it should drain the queued prompt onto this conversation's
    // branch as a `AgentPromptSteered` event so it rides the same
    // `AgentPromptCreated` as the tool results — instead of waiting
    // for full `Idle` and starting a separate turn.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_agents.insert("sp-x".into(), cid.clone());

    let edit_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("a.txt").display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("start_line".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("end_line_exclusive".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("newText".to_owned()),
                    CborValue::Text("a".to_owned()),
                ),
                (
                    CborValue::Text("context_line".to_owned()),
                    CborValue::Text(String::new()),
                ),
            ])]),
        ),
    ]);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: "sp-x".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            arguments: edit_args,
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("agent response with tool call");

    // The conversation must be in `ToolsRunning` so `submit_user_prompt`
    // takes the queued path rather than dispatching.
    assert!(matches!(
        h.agents.get(&cid).expect("default").turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    let submission = h
        .submit_user_prompt("s1".into(), "redirect".to_owned())
        .expect("submit");
    assert!(
        matches!(submission, PromptSubmission::Queued),
        "in-flight turn should force queueing, got {submission:?}"
    );
    assert_eq!(
        h.agents.get(&cid).expect("default").pending_prompts.len(),
        1,
        "the steering message should sit in pending_prompts until the next-round seam",
    );

    drive_harness_until_call_completes(&mut h, "c1");

    assert!(
        h.agents
            .get(&cid)
            .expect("default")
            .pending_prompts
            .is_empty(),
        "queued prompt must be drained when folded as a steer",
    );

    // Walk the event log and verify ordering: the AgentPromptSteered
    // is published before the next-round AgentPromptCreated, and the
    // latter's `context_items` includes the steered text alongside the
    // original user prompt.
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut saw_steered = false;
    let mut saw_next_round = false;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        match &entry.event {
            Event::AgentPromptSteered(steered) => {
                assert_eq!(steered.text, "redirect");
                assert!(
                    !saw_next_round,
                    "steered event must precede the prompt it folds into",
                );
                saw_steered = true;
            }
            Event::AgentPromptCreated(p) if saw_steered => {
                assert!(
                    saw_steered,
                    "next-round prompt must follow the AgentPromptSteered",
                );
                saw_next_round = true;

                let user_texts: Vec<String> = p
                    .context
                    .flatten()
                    .iter()
                    .filter_map(|item| match item {
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) => text_part(item).map(str::to_owned),
                        _ => None,
                    })
                    .collect();
                assert!(
                    user_texts.iter().any(|t| t == "redirect"),
                    "next-round prompt should fold the steered message into messages; \
                     user texts were {user_texts:?}",
                );

                // The steered message must land *after* the tool result
                // on the same branch — otherwise the model sees its
                // tool_use replied to with a steer instead of the
                // ToolResult, which providers reject.
                let last_tool_result_idx = p
                    .context
                    .flatten()
                    .iter()
                    .rposition(|item| matches!(item, ContextItem::ToolResult(_)));
                let last_user_idx = p.context.flatten().iter().rposition(|item| {
                    matches!(
                        item,
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) if text_part(item) == Some("redirect")
                    )
                });
                assert!(
                    last_tool_result_idx.is_some(),
                    "next-round prompt must include the tool result"
                );
                assert!(
                    matches!((last_tool_result_idx, last_user_idx),
                        (Some(t), Some(u)) if u > t),
                    "steered user message must follow the tool result, not precede it",
                );
            }
            _ => {}
        }
    }
    assert!(saw_steered, "expected a AgentPromptSteered event");
    assert!(saw_next_round, "expected the next-round AgentPromptCreated");

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_calls_stop_reason_without_tool_items_does_not_wedge_turn() {
    // Providers can disagree between their terminal stop reason and
    // emitted item list. With no concrete tool-call items, there is no
    // round Tau can execute, so the harness must finish this model call
    // instead of entering an empty ToolsRunning state.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "hello".to_owned())
        .expect("submit");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: read_nth_prompt_created(&h, 0).agent_prompt_id,
        agent_id: read_nth_prompt_created(&h, 0).agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
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
    })
    .expect("finish");

    let cid = ensure_test_user_agent(&mut h);
    assert!(matches!(
        h.agents.get(&cid).expect("default").turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(h.tool_turn.pending_len(), 0);

    h.submit_user_prompt("s1".into(), "again".to_owned())
        .expect("submit again");
    assert!(matches!(
        h.agents.get(&cid).expect("default").turn_state,
        AgentTurnState::AgentThinking { .. }
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn agent_prompt_created_uses_refs_for_linear_extension() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    append_user_message_via_event(&mut h, "s1", "again");
    let spid2 = h.send_prompt_to_agent("s1");
    let raw2 = read_raw_prompt_created(&h, &spid2);
    let prompt2 = read_prompt_created(&h, &spid2);
    assert!(raw2.tools_ref.is_none());
    assert_eq!(raw2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(raw2.context.flatten(), prompt2.context.flatten());
    assert_eq!(prompt2.tools, prompt1.tools);

    h.shutdown().expect("shutdown");
}

#[test]
fn linear_agent_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.model_params, prompt1.model_params);
    assert!(
        prompt1.context.flatten().len() < prompt2.context.flatten().len(),
        "second prompt should strictly extend first: {} !< {}",
        prompt1.context.flatten().len(),
        prompt2.context.flatten().len()
    );
    assert_eq!(
        &prompt2.context.flatten()[..prompt1.context.flatten().len()],
        prompt1.context.flatten().as_slice(),
        "second prompt must keep first prompt context items as an exact prefix"
    );

    h.shutdown().expect("shutdown");
}

/// When the agent reports a `response_id` on a finished turn, the
/// next `AgentPromptCreated` for that conversation must carry a
/// `previous_response_candidate` pointing back at it — that's the hook the
/// Responses backend uses to switch into stateful-chain mode and
/// send just the delta upstream. `next_item_index` must equal the
/// assembled item count at the moment the anchor was captured,
/// so the delta slice is exactly the items added since.
#[test]
fn response_id_anchors_next_prompt_with_previous_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn chained_sub_chunk_cacheable_tokens_does_not_emit_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (Some(500), Some(0), None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);
    let spid2 = prompt2.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid2,
        agent_id: prompt2.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "second answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (Some(500), Some(0), None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_def".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish second");

    let mut cursor = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        assert!(
            !matches!(entry.event, Event::ProviderCacheMissDiagnostic(_)),
            "sub-cache-chunk turn must not emit cache miss diagnostic"
        );
    }

    h.shutdown().expect("shutdown");
}

/// Switching `selected_model` mid-conversation must bust the chain.
/// The prior response was produced by a different model — its
/// stored state on the upstream API is meaningless for the new
/// model, and sending `previous_response_id` would either error or
/// silently mix incompatible reasoning.
#[test]
fn model_switch_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model-a".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // The selected role resolves to a different model.
    h.selected_model = Some("test/model-b".into());

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

/// Changing role-derived model parameters mid-conversation must bust the chain.
/// The Codex Responses upstream stored its reasoning state against
/// the *previous* turn's effort/verbosity/thinking-summary; sending
/// a `previous_response_id` from a request whose non-input fields
/// drifted would silently decohere the model's reasoning. The
/// fingerprint check catches this before the round-trip — mirrors
/// Pi's `requestBodiesMatchExceptInput`.
#[test]
fn params_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.available_roles
        .get_mut(&h.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::Effort::Low);

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_abc".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // User dials effort up between turns by updating the selected role override.
    h.available_roles
        .get_mut(&h.selected_role.clone())
        .expect("selected role")
        .effort = Some(tau_proto::Effort::High);

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// A skill loading mid-conversation (and surfacing into the system
/// prompt) must also bust the chain — the upstream stored its
/// reasoning state against the *previous* system prompt, and
/// chaining a request whose `instructions` field has new content
/// would silently mix the skill's guidance with reasoning that
/// never saw it. This is the more likely real-world trigger for a
/// fingerprint miss than a manual role-parameter flip: skills
/// auto-load as the agent works.
#[test]
fn system_prompt_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_skills".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // Simulate a skill becoming visible in the system prompt between
    // turns. `build_system_prompt` renders any `add_to_prompt: true`
    // skill into the prompt body, so inserting one here is the
    // narrowest way to make the system_prompt string drift without
    // touching unrelated state.
    h.discovered_skills.insert(
        tau_proto::SkillName::new("late-loaded"),
        crate::discovery::DiscoveredSkill {
            source_id: tau_proto::ConnectionId::from("test-ext"),
            description: "appears between turns".to_owned(),
            source: crate::discovery::DiscoveredSkillSource::File(std::path::PathBuf::from(
                "/tmp/late-loaded.md",
            )),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: false,
            modified: None,
        },
    );

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// A tool registering mid-conversation must bust the chain — the
/// upstream stored its reasoning state against the *previous* tools
/// list, and chaining a request whose `tools` field grew (or shrank)
/// would silently mix new affordances into reasoning that never saw
/// them. Realistic trigger: an extension hot-registers a tool while
/// the user is mid-task.
#[test]
fn tools_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: Some("resp_tools".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // A new tool appears between turns — same shape as an extension
    // hot-registering. `gather_tool_definitions` reads from the
    // registry on every send, so the next prompt's `tools` field
    // grows by one.
    h.registry.register(
        "test-ext",
        ToolSpec {
            name: ToolName::new("late_tool"),
            model_visible_name: None,
            description: Some("appeared between turns".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// Counterpart: when the per-request fingerprint inputs *don't*
/// change between turns, the chain anchor must remain valid. Locks
/// in the "compute fingerprint over (system_prompt, tools, params)"
/// surface — if a future change quietly mixes in some other input
/// that drifts across turns (e.g. cwd, current date, session id),
/// this test starts failing.
#[test]
fn stable_params_preserve_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_xyz".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// A turn that didn't yield a `response_id` (Chat Completions
/// backend, an error, etc.) must NOT anchor a chain. The next prompt
/// has to be a full replay — pretending we have a chain we don't
/// would make the upstream API reject the next call.
#[test]
fn missing_response_id_leaves_chain_unset() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    h.submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    h.submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_prompt_extends_completed_first_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let first = h
        .submit_user_prompt("s1".into(), "first".to_owned())
        .expect("submit first");
    assert_eq!(first, PromptSubmission::Dispatched);
    let first_agent_id = h
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conv| conv.agent_id.clone())
        .expect("first prompt agent id");
    publish_pending_agent_context_ready(&mut h, first_agent_id.as_str());
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();

    let second = h
        .submit_user_prompt("s1".into(), "second".to_owned())
        .expect("submit second");
    assert_eq!(second, PromptSubmission::Queued);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    let prompt2 = read_nth_prompt_created(&h, 1);
    assert!(
        prompt1.context.flatten().len() < prompt2.context.flatten().len(),
        "queued follow-up should extend the first prompt"
    );
    assert_eq!(
        &prompt2.context.flatten()[..prompt1.context.flatten().len()],
        prompt1.context.flatten().as_slice()
    );
    let prompt2_items = prompt2.context.flatten();
    let last = prompt2_items.last().expect("last item");
    assert!(matches!(
        last,
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            ..
        })
    ));
    assert_eq!(text_part(last), Some("second"));

    h.shutdown().expect("shutdown");
}

#[test]
fn restore_notice_elapsed_format_uses_minutes_hours_and_days() {
    // The restore notice is model-visible hidden context, so keep the elapsed
    // wording compact and deterministic while still warning about outside
    // changes since the durable transcript stopped.
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(59)))
            .contains("Less than 1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(60)))
            .contains("1 minute has passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(42 * 60)))
            .contains("42 minutes have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(2 * 60 * 60)))
            .contains("2 hours have passed since the last recorded session event")
    );
    assert!(
        restore_notice_prompt_for_elapsed(Some(Duration::from_secs(3 * 24 * 60 * 60)))
            .contains("3 days have passed since the last recorded session event")
    );
}

/// Regression: a cold-resumed session needs one hidden restore notice in the
/// first provider prompt, but startup itself must not send that notice as a
/// standalone turn or as prewarm-only context.
#[test]
fn resumed_startup_folds_restore_notice_before_first_user_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let two_hours_ago = tau_proto::UnixMicros::new(
        tau_proto::UnixMicros::now()
            .get()
            .saturating_sub(2 * 60 * 60 * 1_000_000),
    );
    seed_prior_user_message_at(&sp, "before restore", two_hours_ago);

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert!(h.prompt_agents.is_empty());
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_)
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptPrewarmRequested(prewarm)
            if prewarm
                .context.flatten()
                .iter()
                .any(|item| text_part(item).is_some_and(is_restore_notice_prompt_text))
    )));
    assert_eq!(restore_notice_event_count(&h), 0);

    h.submit_user_prompt("s1".into(), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    let notice_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item).is_some_and(is_restore_notice_prompt_text))
        .expect("restore notice in first prompt");
    let user_pos = prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    let notice = restore_notice_context_text(&prompt).expect("restore notice text");

    assert!(notice_pos < user_pos);
    assert!(notice.contains("Previous session was interrupted and restored."));
    assert!(notice.contains("2 hours have passed since the last recorded session event"));
    assert!(notice.contains("state of the world might have changed"));
    assert_eq!(restore_notice_context_count(&prompt), 1);
    assert_eq!(restore_notice_event_count(&h), 1);

    h.shutdown().expect("shutdown");
}

/// The restore notice is a one-shot durable fact. Follow-up prompts and later
/// cold resumes may replay the original notice in history, but must not append
/// another copy.
#[test]
fn restore_notice_is_not_duplicated_by_followups_or_later_resumes() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_prior_user_message(&sp, "before restore");

    let notice = {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");

        h.submit_user_prompt("s1".into(), "first after restore".to_owned())
            .expect("submit first resumed prompt");
        let first_prompt = read_nth_prompt_created(&h, 0);
        let first_spid = first_prompt.agent_prompt_id.clone();
        let notice = restore_notice_context_text(&first_prompt)
            .expect("restore notice")
            .to_owned();
        assert_eq!(restore_notice_context_count(&first_prompt), 1);

        h.handle_provider_response_finished(provider_text_response(
            &first_spid,
            first_prompt.agent_id.clone(),
            "first answer",
        ))
        .expect("finish first prompt");
        h.submit_user_prompt("s1".into(), "second after restore".to_owned())
            .expect("submit second prompt");
        let second_prompt = read_nth_prompt_created(&h, 1);
        assert_eq!(context_text_count(&second_prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&second_prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);

        h.shutdown().expect("shutdown");
        notice
    };
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");

        h.submit_user_prompt("s1".into(), "third after restore".to_owned())
            .expect("submit after second resume");
        let prompt = read_nth_prompt_created(&h, 0);
        assert_eq!(context_text_count(&prompt, notice.as_str()), 1);
        assert_eq!(restore_notice_context_count(&prompt), 1);
        assert_eq!(restore_notice_event_count(&h), 1);

        h.shutdown().expect("shutdown");
    }
}

/// Regression: a background placeholder without a later background result/error
/// means the real tool was lost across cold restore. Resume must publish a
/// durable background error, fold an internal interruption note before the next
/// user prompt, and let `wait` consume the restored error instead of hanging.
#[test]
fn resumed_lost_background_tool_gets_error_and_wait_returns() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-bg", "slow_bg");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let notice = restored_background_notice("lost-bg");

    assert_eq!(background_error_count(&h, "lost-bg"), 1);
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "lost-bg" && error.message == notice
        )
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_)
    )));

    h.submit_user_prompt("s1".into(), "after restore".to_owned())
        .expect("submit first resumed prompt");
    let first_prompt = read_nth_prompt_created(&h, 0);
    let first_spid = first_prompt.agent_prompt_id.clone();
    let notice_pos = first_prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some(notice.as_str()))
        .expect("background interruption notice in first prompt");
    let user_pos = first_prompt
        .context
        .flatten()
        .iter()
        .position(|item| text_part(item) == Some("after restore"))
        .expect("user prompt in first prompt");
    assert!(notice_pos < user_pos);

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: first_spid,
        agent_id: first_prompt.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-lost-bg".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("lost-bg".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("wait for restored background call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-lost-bg" && error.message == notice
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: restored background notices are owned by the agent whose
/// background call was repaired. A session-level queue let the first prompted
/// agent consume every restored background notice, including notices for other
/// loaded agents in the same session.
#[test]
fn restored_background_notices_are_delivered_to_owning_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder_for_agent(&sp, "main", "main-lost-bg", "slow_bg");
    seed_background_placeholder_for_agent(&sp, "side_agent", "side-lost-bg", "slow_bg");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let main_notice = restored_background_notice("main-lost-bg");
    let side_notice = restored_background_notice("side-lost-bg");
    let main_cid = h
        .agent_routes
        .get("main")
        .cloned()
        .expect("main agent route");
    let side_cid = h
        .agent_routes
        .get("side_agent")
        .cloned()
        .expect("side agent route");

    let side_prompts = h.take_pending_restore_prompts_for_user_prompt(&side_cid);
    assert!(
        side_prompts.iter().any(|prompt| prompt.text == side_notice),
        "side agent should receive its own restored background notice"
    );
    assert!(
        side_prompts.iter().all(|prompt| prompt.text != main_notice),
        "side agent must not receive main agent's restored background notice"
    );

    let main_prompts = h.take_pending_restore_prompts_for_user_prompt(&main_cid);
    assert!(
        main_prompts.iter().any(|prompt| prompt.text == main_notice),
        "main agent should still receive its own restored background notice"
    );
    assert!(
        main_prompts.iter().all(|prompt| prompt.text != side_notice),
        "main agent must not receive side agent's restored background notice"
    );

    h.shutdown().expect("shutdown");
}

/// Resume should treat existing background results/errors as terminal. They are
/// replayed into the wait tracker, but no restored interruption error is
/// appended over the real outcome.
#[test]
fn resume_keeps_existing_background_completions() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "finished-bg", "slow_bg");
    seed_background_placeholder(&sp, "failed-bg", "slow_bg");
    seed_background_result(&sp, "finished-bg", "slow_bg", "finished");
    seed_background_error(&sp, "failed-bg", "slow_bg", "real failure");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");

    assert_eq!(background_result_count(&h, "finished-bg"), 1);
    assert_eq!(background_error_count(&h, "finished-bg"), 0);
    assert_eq!(background_error_count(&h, "failed-bg"), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.message == restored_background_notice(error.call_id.as_str())
    )));

    h.shutdown().expect("shutdown");
}

/// Completed background results restored from the agent log should be
/// available to `wait({})`, not only to exact-id waits.
#[test]
fn resumed_completed_background_result_can_be_consumed_by_no_arg_wait() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-any", "slow_bg");
    seed_background_result(&sp, "restored-any", "slow_bg", "restored output");

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    h.submit_user_prompt("s1".into(), "collect restored background".to_owned())
        .expect("submit first resumed prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: prompt.agent_prompt_id,
        agent_id: prompt.agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-restored-any".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("wait on restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-any"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-any")
                && cbor_map_text(&result.result, "output") == Some("restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// Restored no-arg waits must replay completions by durable completion order,
/// not by the earlier provider-placeholder order.
#[test]
fn resumed_no_arg_wait_uses_restored_completion_event_order() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "restored-a", "slow_bg");
    seed_background_placeholder(&sp, "restored-b", "slow_bg");
    for (call_id, text) in [
        ("restored-b", "first restored output"),
        ("restored-a", "second restored output"),
    ] {
        seed_background_result(&sp, call_id, "slow_bg", text);
    }

    let mut h =
        quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-first"),
        ToolName::new("wait"),
    )
    .expect("consume first restored completion");
    h.handle_wait_tool_call(
        &cid,
        &wait_no_args_call("wait-restored-second"),
        ToolName::new("wait"),
    )
    .expect("consume second restored completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-first"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-b")
                && cbor_map_text(&result.result, "output") == Some("first restored output")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-restored-second"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some("restored-a")
                && cbor_map_text(&result.result, "output") == Some("second restored output")
    )));

    h.shutdown().expect("shutdown");
}

/// The restored background error is durable. A later cold resume must observe
/// the existing error and avoid appending a duplicate.
#[test]
fn repeated_resume_does_not_duplicate_background_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_background_placeholder(&sp, "lost-once", "slow_bg");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");
        assert_eq!(background_error_count(&h, "lost-once"), 1);
        h.shutdown().expect("shutdown");
    }
}

#[test]
fn switch_session_clears_loaded_agents_until_next_prompt() {
    // `/session new` changes the session container. Agents are durable members
    // of one session, so switching clears live agent routing; the next prompt
    // creates a fresh durable agent in the new session.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"
    h.selected_model = Some("test/model".into());
    let model: tau_proto::ModelId = "test/model".into();
    h.current_session_state.context_input_tokens = Some(92_000);
    h.current_session_state.context_cached_tokens = Some(90_000);
    h.current_session_state.context_percent_used = Some(92);
    h.current_session_state.token_usage.start_request(&model);
    h.current_session_state
        .token_usage
        .add_sent(&model, 819_300, 750_000);
    h.current_session_state
        .token_usage
        .add_received(&model, 34_000);

    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(h.agents[&cid].session_id.as_str(), "s1");
    h.agents
        .get_mut(&cid)
        .expect("default conversation")
        .agent_id = Some("old-agent".to_owned());
    h.agent_routes.insert("old-agent".to_owned(), cid.clone());

    let shell_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch");

    let mut saw_session_dir = false;
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if let Event::HarnessSessionDir(session_dir) = &entry.event
            && session_dir.session_id == "s2"
            && session_dir.path.ends_with("s2")
            && session_dir.status == tau_proto::SessionDirStatus::New
        {
            saw_session_dir = true;
        }
    }
    assert!(saw_session_dir, "switch must announce the new session dir");

    assert_eq!(h.current_session_id.as_str(), "s2");
    assert_eq!(h.current_session_state.context_input_tokens, None);
    assert_eq!(h.current_session_state.context_cached_tokens, None);
    assert_eq!(h.current_session_state.context_percent_used, None);
    assert_eq!(
        h.current_session_state.token_usage,
        tau_proto::TokenUsageStats::default()
    );
    assert!(h.agents.is_empty());
    assert!(h.agent_routes.is_empty());

    // Drive the new session through init so submit_user_prompt
    // actually dispatches (rather than queuing).
    h.handle_extension_event(
        &shell_conn,
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s2".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            },
        )),
    )
    .expect("ready");

    let submission = h
        .submit_user_prompt("s2".into(), "hello".to_owned())
        .expect("submit");
    assert_eq!(submission, PromptSubmission::Dispatched);
    let new_cid = test_user_agent(&h);
    let new_agent_id = h.agents[&new_cid]
        .agent_id
        .clone()
        .expect("new session agent id");
    publish_pending_agent_context_ready(&mut h, new_agent_id.as_str());
    assert!(read_nth_prompt_created(&h, 0).agent_id.as_str() == new_agent_id);

    h.shutdown().expect("shutdown");
}

#[test]
fn manual_compact_appends_trigger_and_dispatches_normal_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    enable_remote_compaction_for_test_model(&mut h);

    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = h.agents[&cid].agent_id.clone().expect("durable agent id");
    let selected_role = h.selected_role.clone();
    h.available_roles
        .get_mut(&selected_role)
        .expect("selected role")
        .compaction = Some(tau_config::settings::RoleCompaction::Threshold(1200));

    h.handle_compact_request("s1".into(), Some(&target_agent_id));

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentCompactionTriggered(triggered)
            if triggered.agent_id.as_str() == target_agent_id.as_str()
    )));
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut prompt = None;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if let Event::AgentPromptCreated(created) = entry.event {
            prompt = Some(created);
        }
    }
    let prompt = prompt.expect("normal prompt created");
    assert!(
        prompt
            .context
            .flatten()
            .contains(&ContextItem::CompactionTrigger)
    );
    assert_eq!(
        prompt.compaction,
        Some(tau_proto::PromptCompactionContext {
            compact_threshold: Some(1200),
        })
    );

    h.shutdown().expect("shutdown");
}

fn enable_remote_compaction_for_test_model(h: &mut Harness) {
    h.selected_model = Some("test/model".into());
    h.provider_model_info.insert(
        "test/model".into(),
        tau_proto::ProviderModelInfo {
            id: "test/model".into(),
            display_name: None,
            tags: Vec::new(),
            default_affinity: 0,
            context_window: 1_000,
            efforts: vec![tau_proto::Effort::Medium],
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: true,
        },
    );
}

fn instant_background_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Instant),
        examples: Vec::new(),
    }
}

fn active_prompt_for(h: &Harness, cid: &AgentId) -> AgentPromptId {
    h.agents
        .get(cid)
        .expect("conversation exists")
        .in_flight_prompt
        .clone()
        .expect("active prompt")
}

fn start_background_tool_and_finish_placeholder_turn(
    h: &mut Harness,
    cid: &AgentId,
    call_id: &str,
    tool_name: &str,
) {
    let agent_id = h
        .ensure_agent_id_for_agent(cid)
        .unwrap_or_else(|| "main".to_owned());
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: crate::parse_agent_id(&agent_id),
            text: format!("run {tool_name}"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let spid: AgentPromptId = format!("sp-{call_id}").into();
    seed_agent_thinking(h, cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("start background tool");
    assert!(h.tool_turn.is_backgrounded(&ToolCallId::from(call_id)));

    let placeholder_followup = active_prompt_for(h, cid);
    h.handle_provider_response_finished(provider_text_response(
        &placeholder_followup,
        durable_agent_id_for_conversation(h, cid),
        "placeholder acknowledged",
    ))
    .expect("finish placeholder followup");
    assert!(matches!(
        h.agents.get(cid).expect("conversation exists").turn_state,
        AgentTurnState::Idle
    ));
}

#[test]
fn wait_returns_internal_background_error_after_extension_disconnect() {
    // A backgrounded call belongs to its call id, not to a future provider
    // registration. When the extension disconnects, `wait` must consume the
    // synthesized background error immediately instead of hanging.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-bg-disconnect");
    h.registry.register(
        "conn-bg-disconnect",
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-disconnect".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_disconnect",
    );
    assert_eq!(
        h.pending_tool_providers
            .get(&call_id)
            .map(|provider| provider.as_str()),
        Some("conn-bg-disconnect")
    );

    h.handle_disconnect("conn-bg-disconnect");

    let expected = extension_disconnected_background_tool_call_error_message(&call_id);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == call_id.as_str()
                && error.message == expected
    )));

    let _replacement_events = connect_test_tool(&mut h, "conn-bg-replacement");
    h.registry.register(
        "conn-bg-replacement",
        instant_background_test_tool_spec("slow_disconnect"),
    );

    let wait_call = AgentToolCall {
        id: "wait-bg-disconnect".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait returns disconnected background error");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-bg-disconnect"
                && error.message == expected
    )));

    h.shutdown().expect("shutdown");
}

/// A no-arg wait that is already blocked when its background call completes
/// must consume the result and suppress the normal internal completion prompt.
#[test]
fn no_arg_wait_before_background_completion_suppresses_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-bg-any-before");
    h.registry.register(
        "conn-bg-any-before",
        instant_background_test_tool_spec("slow_any_before"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-any-before".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        call_id.as_str(),
        "slow_any_before",
    );

    let wait_call = wait_no_args_call("wait-any-before");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start no-arg wait");
    h.handle_extension_event_inner(
        "conn-bg-any-before",
        Event::ToolResult(final_tool_result(
            call_id.as_str(),
            "slow_any_before",
            "background done",
        )),
    )
    .expect("background result");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-before"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("background done")
    )));
    let completion_prompt = background_completion_prompt(&call_id);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == completion_prompt
    )));
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// If a completion notice is queued but not steered yet, `wait({})` should
/// remove it while returning the already-completed background result.
#[test]
fn no_arg_wait_after_background_completion_removes_queued_completion_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-any-after".into();

    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.record_wait_background_result(tau_proto::ToolBackgroundResult {
        call_id: call_id.clone(),
        tool_name: ToolName::new("slow_any_after"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("already done".to_owned()),
        originator: tau_proto::PromptOriginator::User,

        display: None,
    });
    seed_tools_running(&mut h, &cid, Vec::new());
    h.queue_background_completion_prompt(&cid, &call_id);
    let completion_prompt = background_completion_prompt(&call_id);
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == completion_prompt && prompt.is_internal())
    );

    let wait_call = wait_no_args_call("wait-any-after");
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("consume queued completion");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "wait-any-after"
                && cbor_map_text(&result.result, "original_tool_call_id") == Some(call_id.as_str())
                && cbor_map_text(&result.result, "output") == Some("already done")
    )));
    assert!(
        h.suppressed_background_completion_prompts
            .contains(&call_id)
    );
    assert!(
        h.agents[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != completion_prompt)
    );

    h.shutdown().expect("shutdown");
}

/// Agent-to-agent messages are real input for the recipient. If the recipient
/// is blocked in `wait`, the wait must return immediately so the hidden message
/// can be folded into the next prompt instead of being stuck behind a passive
/// wait for background work.
#[test]
fn agent_message_interrupts_recipient_active_wait() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-msg-wait");
    h.registry.register(
        "conn-msg-wait",
        instant_background_test_tool_spec("slow_msg_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agents[&cid].agent_id.clone().expect("recipient id");
    let background_call_id: ToolCallId = "bg-msg-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_msg_wait",
    );

    let wait_call_id: ToolCallId = "wait-msg-interrupt".into();
    let wait_call = AgentToolCall {
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);

    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "test-message-interrupts-wait".into(),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            message: "please stop waiting".to_owned(),
        }),
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.agent_id.as_str() == recipient_id.as_str()
                && steered.text.contains("please stop waiting")
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: a user prompt can queue while another foreground tool from the
/// same provider turn is still running, before the model's later `wait` call is
/// dispatched. Starting that `wait` must notice the already-queued user input
/// and complete immediately instead of parking the agent behind background
/// work.
#[test]
fn wait_start_is_interrupted_by_already_queued_user_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-queued-wait");
    h.registry.register(
        "conn-queued-wait",
        instant_background_test_tool_spec("slow_queued_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let background_call_id: ToolCallId = "bg-queued-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_queued_wait",
    );
    h.agents
        .get_mut(&cid)
        .expect("agent")
        .pending_prompts
        .push_back(PendingPrompt::user("user input already queued".to_owned()));

    let wait_call_id: ToolCallId = "wait-queued-input".into();
    let wait_call = wait_no_args_call(wait_call_id.as_str());
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait interrupted by queued user input");

    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "wait should complete exactly once when queued user input preempts it"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text == "user input already queued"
    )));

    h.handle_extension_event_inner(
        "conn-queued-wait",
        Event::ToolResult(final_tool_result(
            background_call_id.as_str(),
            "slow_queued_wait",
            "background done after interrupt",
        )),
    )
    .expect("background result after interrupted wait");
    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "the later background result must not resume a wait that never started"
    );

    h.shutdown().expect("shutdown");
}

/// Regression companion to the user-prompt queued-before-wait race: inbound
/// `agent.message_received` prompts are hidden/internal in transcript terms,
/// but active waits already treat them as input that must interrupt waiting.
/// The same preemption must apply when the message is queued before `wait`
/// starts.
#[test]
fn wait_start_is_interrupted_by_already_queued_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-queued-message-wait");
    h.registry.register(
        "conn-queued-message-wait",
        instant_background_test_tool_spec("slow_queued_message_wait"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let background_call_id: ToolCallId = "bg-queued-message-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &cid,
        background_call_id.as_str(),
        "slow_queued_message_wait",
    );
    h.agents
        .get_mut(&cid)
        .expect("agent")
        .pending_prompts
        .push_back(PendingPrompt::agent_message_received(
            "queued manager message".to_owned(),
        ));

    let wait_call_id: ToolCallId = "wait-queued-message".into();
    let wait_call = wait_no_args_call(wait_call_id.as_str());
    seed_tools_running(&mut h, &cid, vec![wait_call_id.clone()]);
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("wait interrupted by queued agent message");

    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "wait should complete exactly once when queued agent input preempts it"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "queued manager message"
    )));

    h.handle_extension_event_inner(
        "conn-queued-message-wait",
        Event::ToolResult(final_tool_result(
            background_call_id.as_str(),
            "slow_queued_message_wait",
            "background done after message interrupt",
        )),
    )
    .expect("background result after interrupted wait");
    assert_eq!(
        tool_result_count(&h, wait_call_id.as_str()),
        1,
        "the later background result must not resume a wait that never started"
    );

    h.shutdown().expect("shutdown");
}

/// Exact `wait` is scoped to the background call owner before any waiter is
/// registered. A cross-owner wait should fail immediately rather than creating
/// active wait state that later messages could interrupt.
#[test]
fn cross_owner_exact_wait_is_rejected_without_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-cross-msg-wait");
    h.registry.register(
        "conn-cross-msg-wait",
        instant_background_test_tool_spec("slow_cross_msg_wait"),
    );

    let target_cid = ensure_test_user_agent(&mut h);
    let waiter_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let target_agent_id = h.agents[&target_cid]
        .agent_id
        .clone()
        .expect("target agent id");
    let waiter_agent_id = h.agents[&waiter_cid]
        .agent_id
        .clone()
        .expect("waiter agent id");
    h.pending_agent_context_ready
        .remove(&tau_proto::AgentId::parse(&waiter_agent_id).expect("agent id"));

    let background_call_id: ToolCallId = "bg-cross-msg-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &target_cid,
        background_call_id.as_str(),
        "slow_cross_msg_wait",
    );

    let wait_call_id: ToolCallId = "wait-cross-msg-interrupt".into();
    let wait_call = AgentToolCall {
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call_id.to_string()),
        )]),
    };
    h.agents
        .get_mut(&waiter_cid)
        .expect("waiter agent")
        .pending_prompts
        .push_back(PendingPrompt::user("queued waiter input".to_owned()));
    h.handle_wait_tool_call(&waiter_cid, &wait_call, ToolName::new("wait"))
        .expect("reject cross-owner wait before queued-input preemption");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == wait_call_id.as_str()
                && error.message == "unknown tool call: `bg-cross-msg-wait`"
    )));

    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "test-message-to-target-owner".into(),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&target_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            message: "target owner only".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));

    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "test-message-to-wait-owner".into(),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&waiter_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            message: "waiter should resume".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("interrupted because new input is queued"))
    )));
    h.shutdown().expect("shutdown");
}

/// While a parent's `agent_start` tool call is in flight, the harness
/// must still dispatch the spawned side conversation's prompt
/// immediately — the parent's `ToolsRunning` turn state is logically
/// independent from the side conv's own turn. The two failure modes
/// this test pins down: (1) the side prompt gets queued behind the
/// parent's pending tool result and never goes out (deadlock), and
/// (2) the parent's `ToolsRunning` state gets clobbered when the
/// side conv finishes, leaving the parent unable to receive its
/// `ToolResult`. Uses the real delegate shape (`tool_call_id: Some`).
#[test]
fn start_agent_request_dispatches_while_tool_is_running_and_restores_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("side_source"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = "sp-main".into();
    h.prompt_agents.insert(spid.clone(), cid.clone());
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: target_agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("side_source"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert!(matches!(h.turn_state, TurnState::Idle));
    let default_turn = &h
        .agents
        .get(&test_user_agent(&h))
        .expect("default conversation")
        .turn_state;
    assert!(matches!(default_turn, AgentTurnState::ToolsRunning { .. }));
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_cid = ext_query_cid(&h, "q1").expect("side conversation");
    let side_agent_id = h
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.agent_id.clone())
        .expect("side agent id");
    assert!(
        h.agents
            .values()
            .all(|conv| conv.pending_prompts.is_empty()),
        "side prompt must dispatch immediately"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.text == "side task"
                && prompt.agent_id.as_str() == side_agent_id.as_str()
    )));
    assert!(matches!(h.turn_state, TurnState::Idle));

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: crate::parse_agent_id(&side_agent_id),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "delegated answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q1".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side finished");

    assert!(matches!(h.turn_state, TurnState::Idle));
    assert!(
        h.tool_turn.is_in_flight(&ToolCallId::from("delegate-call")),
        "parent agent_start tool must remain in flight until its ToolResult arrives"
    );
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q1" => Some(result),
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "delegated answer");

    let side_cid = h
        .agent_routes
        .get(&side_agent_id)
        .expect("completed delegate remains targetable")
        .clone();
    let side_conv = h
        .agents
        .get(&side_cid)
        .expect("completed delegate conversation is kept");
    // Tool-backed delegates are detached rather than removed after their tool
    // result is returned so a resumed UI agent can receive follow-up prompts on
    // the same branch without being treated as an extension side query again.
    assert!(matches!(
        side_conv.originator,
        tau_proto::PromptOriginator::User
    ));
    assert!(side_conv.source_connection.is_none());
    assert!(side_conv.parent_tool_call_id.is_none());
    assert!(side_conv.parent_agent_id.is_none());
    assert_eq!(side_conv.agent_id.as_deref(), Some(side_agent_id.as_str()));
    h.shutdown().expect("shutdown");
}

#[test]
fn delegated_agent_user_interaction_prevents_auto_suspend() {
    // If a UI targets a running delegated agent before its delegated reply is
    // returned, that interaction converts it into a normal active agent. The
    // later delegate completion must not hide it from `/agent switch`.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-user".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");
    let side_cid = ext_query_cid(&h, "q-user").expect("side conversation");
    let side_agent_id = h
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.agent_id.clone())
        .expect("side agent id");

    h.submit_prompt_to_agent("s1".into(), &side_agent_id, "user follow-up".to_owned())
        .expect("user prompt to delegate");

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "delegated answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q-user".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side finished");

    assert!(
        h.agent_routes.contains_key(&side_agent_id),
        "interacted delegate remains targetable"
    );

    h.shutdown().expect("shutdown");
}

/// A side agent that receives `agent.message_received` while its original turn
/// is in flight must process that internal message before teardown. Otherwise
/// the `PromptOriginator::Extension` completion path removes the side
/// conversation and drops the queued delivery.
#[test]
fn side_agent_drains_agent_message_before_extension_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-message".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let (side_spid, side_cid) = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default").then(|| (spid.clone(), prompt_cid.clone()))
        })
        .expect("side prompt id");
    let recipient_id = h
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.agent_id.clone())
        .expect("side agent id");

    h.publish_event(
        Some(HARNESS_CONNECTION_ID),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "test-message".into(),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            message: "please include this".to_owned(),
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "initial answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q-message".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side first response");

    assert!(
        h.agents.contains_key(&side_cid),
        "side conversation must stay alive to process queued agent.message_received"
    );
    assert!(
        delegate_events
            .lock()
            .expect("delegate events")
            .iter()
            .all(|routed| {
                !matches!(
                    peel_inner_event(&routed.frame),
                    Some(Event::StartAgentResult(_))
                )
            }),
        "start result must wait until the message turn completes"
    );
    let message_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid == &side_cid && spid != &side_spid).then_some(spid.clone())
        })
        .expect("message prompt dispatched");
    let prompt = read_prompt_created(&h, &message_spid);
    let serialized = serde_json::to_string(&prompt.context.flatten()).expect("json");
    assert!(serialized.contains("please include this"));

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: message_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "final answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "conn-delegate".into(),
            query_id: "q-message".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side message response");

    assert!(
        h.agents.contains_key(&side_cid),
        "tool-backed side conversation stays targetable after message turn"
    );
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-message" => Some(result),
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "final answer");
    h.shutdown().expect("shutdown");
}

/// Delegated agents are durable, user-addressable agents, not temporary
/// implementation details. Their harness conversation id should therefore be
/// the minted public agent id instead of the old deterministic
/// `start-agent-{extension}-{query}` key.
#[test]
fn start_agent_request_conversation_id_is_public_agent_id() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    for conn_id in ["conn-delegate-a", "conn-delegate-b"] {
        let connection_id: tau_proto::ConnectionId = conn_id.into();
        h.extensions.entries.insert(
            connection_id.clone(),
            crate::extension::ExtensionEntry {
                name: "delegate-ext".to_owned(),
                instance_id: 42.into(),
                connection_id: connection_id.clone(),
                kind: tau_proto::ClientKind::Tool,
                require: true,
                respawn_allowed: true,
                pid: None,
                in_process_thread: None,
                supervised_config: None,
                secrets: std::collections::BTreeMap::new(),
                restart_attempt: 0,
                state: crate::extension::ExtensionState::Ready,
                protocol_io: tau_client::ProtocolIoMeter::default(),
            },
        );
        h.extensions.order.push(connection_id);
    }
    h.handle_start_agent_request("conn-delegate-a", ext_query("q-named"))
        .expect("query");
    let mut side_agents = h.agents.iter().filter(|(_, conv)| {
        matches!(
            &conv.originator,
            tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-named"
        )
    });
    let (cid, conv) = side_agents.next().expect("side agent");
    assert!(side_agents.next().is_none());
    let public_agent_id = conv.agent_id.as_deref().expect("public agent id");
    assert_eq!(cid.as_str(), public_agent_id);
    assert!(!cid.as_str().starts_with("start-agent-"));
    let cid = cid.clone();
    let agent_count = h.agents.len();
    h.handle_start_agent_request("conn-delegate-b", ext_query("q-named"))
        .expect("duplicate query");
    assert_eq!(h.agents.len(), agent_count);
    assert_eq!(
        h.agents
            .get(&cid)
            .and_then(|conv| conv.source_connection.as_deref()),
        Some("conn-delegate-b")
    );
    h.shutdown().expect("shutdown");
}

/// A tool-backed `StartAgentRequest` (`tool_call_id: Some(...)`) is the
/// `agent_start` path: it dispatches *while the parent's tool call is
/// still in flight*, so the parent conv's tip is a `ToolUse` block
/// with no matching `ToolResult` yet. The side conv must therefore
/// fork off the tree root with `head: None`, NOT inherit the
/// parent's branch — otherwise (a) the assembled prompt would carry
/// an orphan `ToolUse` block (provider 400s on unmatched tool_use),
/// and (b) the sub-agent would see the user's framing and might
/// recursively re-delegate the same task. (Contrast with the
/// non-tool path, where `tool_call_id: None` deliberately inherits
/// the parent — see `non_tool_start_agent_request_inherits_parent_branch`.)
#[test]
fn start_agent_request_during_tool_call_branches_off_unresolved_tool_use() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);

    // Tool-backed sub-agents (`tool_call_id: Some(...)`) get a fresh
    // context regardless of whether the parent is mid-tool-call: they
    // see only their own `query.instruction`, never the parent's
    // unresolved `agent_start` tool_use (which would be an orphan ToolUse
    // the provider rejects), and never the user's task framing (which
    // would invite recursive re-delegation).
    let saw_orphan_tool_use = prompt
        .context
        .flatten()
        .iter()
        .any(|item| tool_call_id(item) == Some("delegate-call"));
    assert!(
        !saw_orphan_tool_use,
        "side prompt must not replay the parent's unresolved agent_start tool_use"
    );

    let saw_user_framing = prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("delegate something"))
        )
    });
    assert!(
        !saw_user_framing,
        "side prompt must NOT inherit the user's task framing — sub-agents start with a fresh context"
    );

    let saw_own_instruction = prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item) == Some("side task")
        )
    });
    assert!(
        saw_own_instruction,
        "side prompt should contain the delegated instruction"
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool `StartAgentRequest` (`tool_call_id: None`, e.g.
/// `std-notifications`' idle summary) is **not** a delegate, but it still
/// starts an independent agent log. The child prompt contains only its own
/// instruction and selected role/system/tool setup; it must not inherit the
/// parent transcript branch.
#[test]
fn non_tool_start_agent_request_starts_fresh_agent_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());

    // Drive the user's main conversation through one full
    // user-message → agent-final-response turn so the parent conv has
    // a non-empty history when the idle summary fires.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "find the bug in foo.rs".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");
    let parent_head_before = h.agents.get(&cid).expect("default conv").head;
    assert!(
        parent_head_before.is_some(),
        "parent conv should have advanced its head after the agent's reply",
    );

    // std-notifications-shaped query: no tool_call_id, just an
    // instruction asking the model to summarize.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    // A non-tool start-agent request creates an independent agent log.
    // Parent transcript nodes belong to the parent agent, so the side
    // prompt starts from its own instruction instead of inheriting the
    // parent branch.
    let user_task_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("find the bug in foo.rs"))
        )
    });
    let agent_answer_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("I fixed the off-by-one"))
        )
    });
    let instruction_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item) == Some("Summarize in one sentence.")
        )
    });
    assert!(
        !user_task_present,
        "side prompt must not inherit parent user message: {:?}",
        side_prompt.context.flatten(),
    );
    assert!(
        !agent_answer_present,
        "side prompt must not inherit parent assistant reply: {:?}",
        side_prompt.context.flatten(),
    );
    assert!(
        instruction_present,
        "side prompt must contain the summarize-instruction itself: {:?}",
        side_prompt.context.flatten(),
    );

    // Tool execution is blocked locally by the harness. The provider
    // request must still keep `tool_choice: Auto` so the side query's
    // non-input fields match the parent conv's cached chain.
    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "non-tool start-agent request must preserve wire tool_choice for cache compatibility",
    );

    // The parent conv's head must not have moved sideways because of
    // the side conv's publish — both convs are now downstream of the
    // parent's previous tip, but the side conv folded onto its own
    // child node.
    let parent_head_after = h.agents.get(&cid).expect("default conv").head;
    assert_eq!(
        parent_head_before, parent_head_after,
        "side conv's UserMessage must not advance the parent conv's head",
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool start-agent request (idle-summary path) must not execute
/// tools, but it also must not mutate provider-visible request fields
/// to enforce that policy. It must preserve `tool_choice: Auto`; flipping it
/// to `None` changes the wire request and can defeat provider cache reuse.
#[test]
fn non_tool_start_agent_request_preserves_tool_choice_without_parent_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    // Drive one full main-conv turn through the normal dispatch path
    // so `prompt_fingerprints`/`prompt_models` are populated and
    // `handle_provider_response_finished` actually mints the anchor.
    h.submit_user_prompt("s1".into(), "find the bug in foo.rs".to_owned())
        .expect("submit main");
    let main_prompt = read_nth_prompt_created(&h, 0);
    let main_spid = main_prompt.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: main_prompt.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(responses_backend()),
        provider_response_id: Some("resp_parent".to_owned()),
        ws_pool_delta: None,
    })
    .expect("main response");

    // std-notifications-shaped query — `tool_call_id: None` triggers
    // the `tool_choice: None` branch in `send_prompt_to_agent_for`.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "idle-summary query must preserve the parent's wire tool_choice; the harness enforces no-tool execution locally",
    );
    assert!(
        side_prompt.share_user_cache_key,
        "idle-summary side conv keeps setting the legacy cache-sharing hint for \
         older providers; first-party ChatGPT/Codex ignores it and uses the \
         target agent cache bucket",
    );
}

/// Counterpart to `non_tool_start_agent_request_starts_fresh_agent_branch`.
/// The harness picks `tool_choice` per conversation in
/// `send_prompt_to_agent_for`; if that discriminator ever
/// over-matches (e.g. flips on `originator.is_extension()` alone),
/// delegate sub-agents would receive `tool_choice: "none"` and be
/// unable to call any tool — silently turning every delegated task
/// into a one-shot text response. Asserts the inverse leg: when
/// `tool_call_id: Some(...)`, `ToolChoice::Auto` is preserved.
#[test]
fn delegate_start_agent_request_keeps_tool_choice_auto() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "go".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);
    assert_eq!(
        prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "delegated sub-agent must keep tool access (ToolChoice::Auto)",
    );
    assert!(
        !prompt.share_user_cache_key,
        "delegate sub-agents leave the legacy cache-sharing hint unset; \
         first-party ChatGPT/Codex still uses the target agent cache bucket",
    );

    h.shutdown().expect("shutdown");
}

/// Regression for the `tau-agent-bsjr7t` stall: an in-flight
/// non-tool extension side conversation (idle-summary stuck on a
/// usage-limit retry) must be preempted as soon as the user submits
/// a fresh prompt. Otherwise the agent's single prompt slot keeps
/// burning backoff retries on the side conv while the user waits.
#[test]
fn user_prompt_preempts_in_flight_non_tool_ext_side_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    // Seed an in-flight idle-summary side conv with a previously
    // dispatched spid that's notionally still being retried by the
    // agent.
    h.handle_start_agent_request(
        "conn-notifications",
        StartAgentRequest {
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let (side_cid, side_spid) = h
        .prompt_agents
        .iter()
        .find(|(_, prompt_cid)| {
            h.agents
                .get(*prompt_cid)
                .is_some_and(|conv| !conv.originator.is_user())
        })
        .map(|(spid, cid)| (cid.clone(), spid.clone()))
        .expect("side conv must exist");
    let side_conv = h.agents.get(&side_cid).expect("side conv present");
    assert_eq!(
        side_conv.in_flight_prompt.as_ref(),
        Some(&side_spid),
        "sanity: side conv is mid-flight before user submits",
    );

    // User submits a real prompt — the harness must preempt the
    // side conv (cancel it, free the agent slot) before queueing or
    // dispatching the user's turn.
    h.submit_user_prompt("s1".into(), "interrupting prompt".to_owned())
        .expect("submit user");

    let side_conv = h.agents.get(&side_cid).expect("side conv still tracked");
    assert!(
        side_conv.in_flight_prompt.is_none(),
        "user prompt must clear the side conv's in-flight spid so the agent's \
         prompt slot is free; still set to {:?}",
        side_conv.in_flight_prompt,
    );
    assert!(
        h.canceled_prompts.contains(&side_spid),
        "side conv's spid must be marked canceled so a late response is dropped",
    );
    assert!(
        !h.prompt_agents.contains_key(&side_spid),
        "side conv's spid must be unrouted so the agent's eventual abort \
         doesn't try to publish a finished event into a stale slot",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id.as_str() == side_spid.as_str()
                    && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
        )),
        "preempted side prompt must publish a terminal lifecycle event",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a sub-agent's `Shared` tool call must not be gated by the
/// parent's still-in-flight `Exclusive` `agent_start` call. The parent's
/// delegate only resolves once the sub-agent's tools have run, so a
/// global execution-mode gate produces a self-deadlock — the main
/// symptom we hit in `tau-agent-m2dpw4`'s event log.
#[test]
fn side_conversation_shared_tool_dispatches_through_parent_exclusive_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.registry.register(
        "conn-websearch",
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // Main agent issues `agent_start`, putting an Exclusive call in flight
    // on the default conversation.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    // Delegate extension turns it into an StartAgentRequest; the harness
    // spawns a side conversation and dispatches its prompt.
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    // Sub-agent now responds with a Shared `websearch` call. Without
    // per-conversation gating this would queue forever behind the
    // parent's still-in-flight Exclusive `agent_start`.
    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "websearch-call".into(),
            name: tau_proto::ToolName::new("websearch"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response");

    // The Shared call must have been accepted for the websearch
    // extension. The harness broadcasts `ToolStarted`; the
    // subscribed provider sees that event and starts the tool.
    let saw_routed = websearch_events.lock().expect("ws").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolStarted(invoke)) if invoke.call_id.as_str() == "websearch-call"
        )
    });
    assert!(
        saw_routed,
        "side conversation's Shared tool must dispatch despite parent's in-flight Exclusive delegate"
    );
    assert_eq!(
        h.tool_turn.pending_len(),
        0,
        "no entries should be left queued"
    );

    h.shutdown().expect("shutdown");
}

/// Background tool completion stays with a preserved tool-backed delegate.
///
/// A sub-agent can finish after its foreground receives the synthetic
/// background placeholder while the real tool is still running. Tool-backed
/// delegate agents are now detached instead of removed at completion, so
/// the late completion prompt must remain owned by the delegate conversation;
/// otherwise a resumed delegate could not receive results from tools it
/// started.
#[test]
fn background_completion_from_preserved_delegate_queues_on_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let parent_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate slow work".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("main delegate call");

    let mut query = ext_query("q-bg");
    query.tool_call_id = Some("delegate-call".into());
    h.handle_start_agent_request("conn-delegate", query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg").expect("side conversation");
    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "slow-call"
                && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
    )));

    let followup_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side follow-up prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: followup_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "side answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish side conversation");
    assert!(
        h.agents.contains_key(&side_cid),
        "tool-backed delegate conversation is detached/preserved after completion"
    );
    assert_eq!(h.tool_agents.get("slow-call"), Some(&side_cid));

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolResult(ToolResult {
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool result");

    assert!(event_log_contains(&h, "conn-slow", |event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "slow-call"
                && matches!(&result.result, CborValue::Text(text) if text == "real output")
    )));
    let parent = h
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(
        parent
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&"slow-call".into()))
    );
    assert_eq!(
        h.background_completion_targets.get("slow-call"),
        Some(&side_cid),
        "late background completions stay routed to the preserved delegate"
    );

    h.shutdown().expect("shutdown");
}

/// Background tool completions from removed non-tool side agents are
/// transferred to a live parent/default conversation instead of being lost with
/// the removed conversation.
#[test]
fn background_completion_from_removed_side_conversation_queues_on_parent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-agent");
    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let parent_cid = ensure_test_user_agent(&mut h);
    h.handle_start_agent_request("conn-agent", ext_query("q-removed-bg"))
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-removed-bg").expect("side conversation");
    let call_id: ToolCallId = "removed-slow-call".into();

    h.tool_agents.insert(call_id.clone(), side_cid.clone());
    h.background_completion_targets
        .insert(call_id.clone(), side_cid.clone());
    h.tool_turn
        .record_in_flight_for_test(side_cid.clone(), call_id.clone());
    assert!(h.tool_turn.mark_backgrounded(&call_id));
    h.queue_background_completion_prompt(&side_cid, &call_id);

    h.transfer_background_completion_target_before_teardown(&side_cid);
    h.remove_agent(&side_cid);

    assert!(!h.agents.contains_key(&side_cid));
    assert_eq!(h.tool_agents.get(&call_id), Some(&parent_cid));
    assert_eq!(
        h.background_completion_targets.get(&call_id),
        Some(&parent_cid)
    );

    assert!(
        h.agents.contains_key(&parent_cid),
        "parent conversation remains live"
    );

    h.shutdown().expect("shutdown");
}

/// Canceled side agents must not transfer their inner background tools
/// to the parent. Otherwise a canceled delegate can leak an inner shell
/// completion prompt and make that inner call waitable in the parent
/// conversation.
#[test]
fn canceled_side_conversation_drops_inner_background_completion() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let parent_cid = ensure_test_user_agent(&mut h);
    let parent_agent_id = h.agents[&parent_cid].agent_id.clone().expect("agent id");
    let main_spid: AgentPromptId = "sp-main-cancel".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main-cancel");
    h.prompt_agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate slow work".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: crate::parse_agent_id(&parent_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call-cancel".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("main delegate call");

    let mut query = ext_query("q-bg-cancel");
    query.tool_call_id = Some("delegate-call-cancel".into());
    h.handle_start_agent_request("conn-delegate", query)
        .expect("side query");
    let side_cid = ext_query_cid(&h, "q-bg-cancel").expect("side conversation");
    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    let side_agent_id = h.agents[&side_cid].agent_id.clone().expect("agent id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: crate::parse_agent_id(&side_agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call-cancel".into(),
            name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-bg-cancel".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side tool call");

    h.cancel_start_agent_request("q-bg-cancel", &"delegate-call-cancel".into(), false)
        .expect("cancel delegate");
    assert!(!h.agents.contains_key(&side_cid));
    assert!(!h.tool_agents.contains_key("slow-call-cancel"));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "slow-call-cancel"
                && error.message == "Tool call canceled"
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "slow-call-cancel"
    )));
    assert!(
        !h.background_completion_targets
            .contains_key("slow-call-cancel")
    );

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolResult(ToolResult {
            call_id: "slow-call-cancel".into(),
            tool_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("real output".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool result is ignored");

    assert!(!event_log_contains(&h, "conn-slow", |event| matches!(
        event,
        Event::ToolBackgroundResult(result) if result.call_id.as_str() == "slow-call-cancel"
    )));
    let parent = h
        .agents
        .get(&parent_cid)
        .expect("parent conversation remains live");
    assert!(!parent.pending_prompts.iter().any(|prompt| prompt.text
        == background_completion_prompt(&"slow-call-cancel".into())
        && prompt.is_internal()));

    h.shutdown().expect("shutdown");
}

#[test]
fn background_notification_suppression_keeps_error_event_but_skips_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-fail");
    h.registry.register(
        "conn-fail",
        ToolSpec {
            name: ToolName::new("fail"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-error".into();
    seed_agent_thinking(&mut h, &cid, "sp-bg-error");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run fail".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "fail-call".into(),
            name: ToolName::new("fail"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("background tool call");

    h.suppress_background_completion_prompt("fail-call".into());
    h.handle_extension_event_inner(
        "conn-fail",
        Event::ToolError(tau_proto::ToolError {
            call_id: "fail-call".into(),
            tool_name: ToolName::new("fail"),
            tool_type: tau_proto::ToolType::Function,
            message: "late failure".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
    )
    .expect("late tool error");

    assert!(event_log_contains(&h, "conn-fail", |event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "fail-call" && error.message == "late failure"
    )));
    assert!(!event_log_contains(&h, "conn-fail", |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == "fail-call"
    )));
    let conv = h.agents.get(&cid).expect("conversation remains live");
    assert!(
        conv.pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&"fail-call".into()))
    );

    h.shutdown().expect("shutdown");
}

/// If a wait is interrupted before the background call finishes, unsuppressing
/// first should let the later completion queue the normal internal prompt.
#[test]
fn background_notification_unsuppress_before_completion_allows_later_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-unsuppress-before".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.unsuppress_background_completion_prompt(call_id.clone());

    h.agents
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);

    let conv = h
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// If the real background completion arrives while suppressed, unsuppressing
/// later should restore the completion prompt from the recorded target map.
#[test]
fn background_notification_unsuppress_after_suppressed_completion_queues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-unsuppress-after".into();

    h.suppress_background_completion_prompt(call_id.clone());
    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    assert!(
        h.agents
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.agents
        .get_mut(&cid)
        .expect("default conversation remains live")
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: Vec::new(),
    };
    h.unsuppress_background_completion_prompt(call_id.clone());

    let conv = h
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    assert!(conv.pending_prompts.iter().any(|prompt| {
        prompt.text == background_completion_prompt(&call_id) && prompt.is_internal()
    }));

    h.shutdown().expect("shutdown");
}

/// Completed background calls remain in the target map so repeated wait cycles
/// can remove and then re-add the queued internal completion prompt.
#[test]
fn background_notification_repeated_suppress_unsuppress_after_completion_requeues_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg-repeat".into();

    h.background_completion_targets
        .insert(call_id.clone(), cid.clone());
    h.queue_background_completion_prompt(&cid, &call_id);
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agents
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agents
            .get(&cid)
            .expect("default conversation remains live")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.unsuppress_background_completion_prompt(call_id.clone());
    let conv = h
        .agents
        .get(&cid)
        .expect("default conversation remains live");
    let prompt_count = conv
        .pending_prompts
        .iter()
        .filter(|prompt| prompt.text == background_completion_prompt(&call_id))
        .count();
    assert_eq!(prompt_count, 1);

    h.shutdown().expect("shutdown");
}

/// Late progress for a backgrounded tool must not be published. The foreground
/// tool result has already closed the visible tool block, so orphan progress
/// would render as confusing standalone text like `shell: running shell
/// command`.
#[test]
fn backgrounded_tool_progress_is_not_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let _ = connect_test_tool(&mut h, "conn-slow");
    h.registry.register(
        "conn-slow",
        ToolSpec {
            name: ToolName::new("slow"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-bg-progress".into();
    seed_agent_thinking(&mut h, &cid, "sp-bg-progress");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run slow".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "slow-call".into(),
            name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("background tool call");

    h.handle_extension_event_inner(
        "conn-slow",
        Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "slow-call".into(),
            tool_name: ToolName::new("slow"),
            message: Some("running shell command".to_owned()),
            progress: None,
            display: None,
        }),
    )
    .expect("late progress");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolProgress(progress) if progress.call_id.as_str() == "slow-call"
    )));

    h.shutdown().expect("shutdown");
}

/// Suppression can arrive after a background completion prompt was queued but
/// before the agent saw it; in that case the queued internal prompt is removed.
#[test]
fn background_notification_suppression_removes_queued_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "bg".into();

    h.agents
        .get_mut(&cid)
        .expect("default conversation exists")
        .pending_prompts
        .push_back(PendingPrompt::internal(background_completion_prompt(
            &call_id,
        )));
    assert!(
        h.agents
            .get(&cid)
            .expect("default conversation exists")
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text == background_completion_prompt(&call_id))
    );

    h.suppress_background_completion_prompt(call_id.clone());
    assert!(
        h.agents
            .get(&cid)
            .expect("default conversation exists")
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != background_completion_prompt(&call_id))
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn shared_start_agent_requests_start_concurrently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");

    h.handle_start_agent_request("conn-a", ext_query("q-a"))
        .expect("query a");
    h.handle_start_agent_request("conn-b", ext_query("q-b"))
        .expect("query b");

    assert!(ext_query_cid(&h, "q-a").is_some());
    assert!(ext_query_cid(&h, "q-b").is_some());
    assert!(h.pending_start_agent_requests.is_empty());

    h.shutdown().expect("shutdown");
}

/// Start-agent requests do not use harness-level scheduling; filesystem
/// coordination is handled by ext-shell directory locks.
#[test]
fn start_agent_requests_do_not_block_independent_queries() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");
    let _ = connect_test_tool(&mut h, "conn-d");

    h.handle_start_agent_request("conn-a", ext_query("q-update-a"))
        .expect("update query a");
    h.handle_start_agent_request("conn-b", ext_query("q-shared"))
        .expect("shared query");
    h.handle_start_agent_request("conn-c", ext_query("q-update-b"))
        .expect("update query b");
    h.handle_start_agent_request("conn-d", ext_query("q-exclusive"))
        .expect("exclusive query");

    for query_id in ["q-update-a", "q-shared", "q-update-b", "q-exclusive"] {
        assert!(
            ext_query_cid(&h, query_id).is_some(),
            "{query_id} should start immediately"
        );
    }
    assert!(h.pending_start_agent_requests.is_empty());

    h.shutdown().expect("shutdown");
}

/// Tool-backed nested start-agent requests are independent agents and do not
/// wait on their parent at harness level.
#[test]
fn nested_start_agent_request_starts_independently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request("conn-delegate", ext_query("q-outer"))
        .expect("outer query");
    let outer_cid = ext_query_cid(&h, "q-outer").expect("outer started");

    h.tool_agents
        .insert("nested-call".into(), outer_cid.clone());
    let mut nested = ext_query("q-nested");
    nested.tool_call_id = Some("nested-call".into());
    nested.task_name = Some("nested".to_owned());
    h.handle_start_agent_request("conn-delegate", nested)
        .expect("nested query");

    let nested_cid = ext_query_cid(&h, "q-nested").expect("nested started");
    assert_ne!(outer_cid, nested_cid);
    assert!(h.pending_start_agent_requests.is_empty());

    h.shutdown().expect("shutdown");
}

/// Regression: `agent_start` display names should remain the requested topic.
/// Parent lineage is represented by generic watch state, not encoded into
/// `agent.started.display_name`, because display names nest and are
/// user-visible wherever the agent is referenced.
#[test]
fn tool_backed_start_agent_display_name_does_not_include_parent_lineage() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request("conn-delegate", ext_query("q-parent"))
        .expect("parent query");
    let parent_cid = ext_query_cid(&h, "q-parent").expect("parent started");

    h.tool_agents.insert("child-call".into(), parent_cid);
    let mut child = ext_query("q-child");
    child.tool_call_id = Some("child-call".into());
    child.task_name = Some("fix streaming ellipsis".to_owned());
    h.handle_start_agent_request("conn-delegate", child)
        .expect("child query");

    let child_cid = ext_query_cid(&h, "q-child").expect("child started");
    let display_name = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.agent_id.as_str() == child_cid.as_str() => {
                started.display_name
            }
            _ => None,
        })
        .expect("child display name");

    assert_eq!(display_name, "fix streaming ellipsis");
    assert!(!display_name.contains("child of"));

    h.shutdown().expect("shutdown");
}

/// A wait that is already blocked on a tool call must be released even when the
/// terminal event is a harness-synthesized routing error instead of a provider
/// response. Otherwise `wait` can hang forever after unavailable-tool paths.
#[test]
fn wait_resolves_on_synthetic_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let target_call_id: ToolCallId = "target-call".into();

    h.tool_agents.insert(target_call_id.clone(), cid.clone());
    h.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("missing"),
            internal_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        id: "wait-call".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");

    let missing_message = unavailable_tool_error_message(&ToolName::new("missing"));
    h.publish_terminal_tool_error(
        Some(&cid),
        None,
        tau_proto::ToolError {
            call_id: target_call_id,
            tool_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
            message: missing_message.clone(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        },
    );

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call"
                && error.message == missing_message
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: `wait` is harness-owned and publishes its answer inline, but the
/// answer still must be folded as a provider-terminal tool output. Otherwise
/// the next full replay contains the `wait` ToolCall without a matching
/// ToolResult, which OpenAI rejects with `No tool output found for function
/// call …`.
#[test]
fn wait_tool_reply_is_folded_into_followup_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    append_user_message_via_event(&mut h, "s1", "wait on missing call");
    seed_agent_thinking(&mut h, &cid, "sp-wait");
    let spid: AgentPromptId = "sp-wait".into();
    h.prompt_agents.insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-call".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("missing-target".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("wait response");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id.as_str() == "wait-call"
    )));
    let followup_spid = h
        .prompt_agents
        .iter()
        .find_map(|(prompt_id, prompt_cid)| {
            (prompt_id != &spid && prompt_cid == &cid).then_some(prompt_id.clone())
        })
        .expect("follow-up prompt id");
    let prompt = read_prompt_created(&h, &followup_spid);
    let prompt_items = prompt.context.flatten();
    let tool_uses: Vec<&str> = prompt_items.iter().filter_map(tool_call_id).collect();
    let tool_results: Vec<&str> = prompt_items.iter().filter_map(tool_result_id).collect();

    assert!(
        tool_uses.contains(&"wait-call"),
        "follow-up prompt must include the wait ToolCall; got: {tool_uses:?}",
    );
    assert!(
        tool_results.contains(&"wait-call"),
        "follow-up prompt must include the matching wait ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression for `tau-agent-ral6kd`: a parent `agent_start` call is only a
/// side-agent launcher. It must not keep a normal tool from the same agent turn
/// queued behind it; filesystem locking is handled by ext-shell, not the
/// harness tool-turn queue.
#[test]
fn delegate_launcher_does_not_block_same_turn_exclusive_tool() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-mutate");
    h.registry.register(
        "conn-mutate",
        ToolSpec {
            name: ToolName::new("mutate"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-call".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "mutate-call".into(),
                name: ToolName::new("mutate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    assert!(h.tool_turn.is_in_flight(&ToolCallId::from("delegate-call")),);
    assert_eq!(
        h.tool_turn.pending_len(),
        0,
        "mutating tool must not remain queued behind the delegate launcher",
    );

    h.shutdown().expect("shutdown");
}

/// Mutating tool calls in distinct side conversations dispatch independently.
/// Any real filesystem coordination must happen inside the tool extension.
#[test]
fn mutating_tools_in_distinct_side_conversations_dispatch_concurrently() {
    use tau_proto::CborValue;

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-mutate");
    h.registry.register(
        "conn-mutate",
        ToolSpec {
            name: ToolName::new("mutate"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // The parent creates two realistic side agents concurrently. The assertion
    // below is about mutating tools owned by those distinct side agents.
    let parent_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "fan out".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let delegate_args = CborValue::Map(Vec::new());
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-A".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: delegate_args.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-B".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: delegate_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-A".to_owned(),
            instruction: "side task A".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-A".into()),
            task_name: Some("A".to_owned()),
        },
    )
    .expect("query A");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-B".to_owned(),
            instruction: "side task B".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-B".into()),
            task_name: Some("B".to_owned()),
        },
    )
    .expect("query B");

    let cid_a = h
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-A"
            )
            .then_some(cid.clone())
        })
        .expect("conversation A");
    let cid_b = h
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-B"
            )
            .then_some(cid.clone())
        })
        .expect("conversation B");
    assert_ne!(cid_a, cid_b, "side agents must be distinct");

    let spid_a = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_a).then_some(spid.clone()))
        .expect("prompt A");
    let spid_b = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_b).then_some(spid.clone()))
        .expect("prompt B");

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid_a,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-A".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-A".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response A");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid_b,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-B".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-B".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response B");

    let mut_a_id: ToolCallId = "mut-A".to_owned().into();
    let mut_b_id: ToolCallId = "mut-B".to_owned().into();
    assert!(
        h.tool_turn.is_in_flight(&mut_a_id),
        "conversation A's mutating call should be in flight",
    );
    assert!(
        h.tool_turn.is_in_flight(&mut_b_id),
        "conversation B's mutating call should be in flight too",
    );
    assert_eq!(h.tool_agents.get("mut-A"), Some(&cid_a));
    assert_eq!(h.tool_agents.get("mut-B"), Some(&cid_b));
    assert_ne!(
        h.tool_agents.get("mut-A"),
        h.tool_agents.get("mut-B"),
        "mutating calls must be attributed to different agents",
    );
    assert_eq!(
        h.tool_turn.pending_len(),
        0,
        "cross-conversation mutating calls should not queue behind each other",
    );

    h.shutdown().expect("shutdown");
}

fn start_agent_request_error(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
    query_id: &str,
) -> Option<String> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == query_id => {
                result.error.clone()
            }
            _ => None,
        })
}

fn drain_watches_updated(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
) -> Vec<tau_proto::AgentWatchesUpdated> {
    let mut frames = frames.lock().expect("frames");
    let mut out = Vec::new();
    frames.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::AgentWatchesUpdated(snapshot)) => {
            out.push(snapshot.clone());
            false
        }
        _ => true,
    });
    out
}

fn drain_stats_updated(frames: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<tau_proto::AgentStatsUpdated> {
    let mut frames = frames.lock().expect("frames");
    let mut out = Vec::new();
    frames.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::AgentStatsUpdated(stats)) => {
            out.push(stats.clone());
            false
        }
        _ => true,
    });
    out
}

fn configure_delegate_error_roles(h: &mut Harness) {
    let available_model: tau_proto::ModelId = "test/available".into();
    set_available_provider_models(h, [provider_model_info(available_model.clone(), 128_000)]);
    h.available_roles = std::collections::HashMap::from([
        (
            "beta".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model.clone()),
                ..Default::default()
            },
        ),
        (
            "alpha".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model),
                ..Default::default()
            },
        ),
        (
            "offline".to_owned(),
            tau_config::settings::AgentRole {
                model: Some("test/offline".into()),
                ..Default::default()
            },
        ),
    ]);
}

#[test]
fn delegate_invalid_or_unavailable_role_errors_with_sorted_available_roles() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    configure_delegate_error_roles(&mut h);

    let delegate = connect_test_tool(&mut h, "conn-delegate");
    for (query_id, role, expected_reason) in [
        ("q-missing", "missing", "requested role does not exist"),
        (
            "q-offline",
            "offline",
            "requested role is not backed by an available model",
        ),
    ] {
        h.handle_start_agent_request(
            "conn-delegate",
            StartAgentRequest {
                parent_agent: None,
                query_id: query_id.to_owned(),
                instruction: "side task".to_owned(),
                role: Some(role.to_owned()),
                input_stats: tau_proto::ToolUseStats::default(),
                tool_call_id: Some(format!("delegate-{query_id}").into()),
                task_name: Some(query_id.to_owned()),
            },
        )
        .expect("query");
        let error = start_agent_request_error(&delegate, query_id).expect("query error");
        assert!(error.contains(expected_reason), "got: {error}");
        assert!(
            error.contains("available roles: alpha, beta"),
            "available roles should be sorted and filtered: {error}"
        );
        assert!(
            !error.contains("available roles: alpha, beta, offline"),
            "unavailable role leaked into available role list: {error}"
        );
    }

    h.shutdown().expect("shutdown");
}

/// Omitting `role` on the agent_start tool means `engineer`; if that
/// role cannot resolve to an available model, the harness reports that
/// agent_start default as the problem instead of silently falling back to
/// another role.
#[test]
fn delegate_missing_default_engineer_errors_when_unavailable() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    configure_delegate_error_roles(&mut h);

    let delegate = connect_test_tool(&mut h, "conn-delegate");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-default".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("default".to_owned()),
        },
    )
    .expect("query");

    let error = start_agent_request_error(&delegate, "q-default").expect("query error");
    assert!(
        error.contains(
            "agent_start requires default role `engineer`, but it is not available: `engineer`"
        ),
        "got: {error}"
    );
    assert!(
        error.contains("available roles: alpha, beta"),
        "got: {error}"
    );
    assert!(ext_query_cid(&h, "q-default").is_none());

    h.shutdown().expect("shutdown");
}

#[test]
fn generic_agent_watch_snapshots_replay_and_clear_on_session_switch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let live = connect_test_tool(&mut h, "watch-live");
    h.complete_subscription(
        "watch-live",
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_WATCHES_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_watches_updated(&live);

    h.set_agent_watch(
        "watcher",
        "child-b",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher",
        "child-a",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher",
        "child-b",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    h.set_agent_watch(
        "watcher",
        "child-a",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );

    let snapshots = drain_watches_updated(&live);
    assert_eq!(snapshots.len(), 4);
    assert_eq!(
        snapshots[0].watched_agent_ids,
        vec![crate::parse_agent_id("child-b")]
    );
    assert_eq!(
        snapshots[1].watched_agent_ids,
        vec![
            crate::parse_agent_id("child-a"),
            crate::parse_agent_id("child-b")
        ]
    );
    assert_eq!(
        snapshots[2].watched_agent_ids,
        vec![crate::parse_agent_id("child-a")]
    );
    assert!(snapshots[3].watched_agent_ids.is_empty());

    h.set_agent_watch(
        "watcher",
        "child-a",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let replay = connect_test_tool(&mut h, "watch-replay");
    h.complete_subscription(
        "watch-replay",
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_WATCHES_UPDATED,
        )],
        Vec::new(),
    )
    .expect("subscribe replay");
    let replayed = drain_watches_updated(&replay);
    assert_eq!(replayed.len(), 1);
    assert_eq!(
        replayed[0].cause,
        tau_proto::AgentWatchUpdateCause::SessionSnapshot
    );
    assert_eq!(replayed[0].watcher_id, crate::parse_agent_id("watcher"));
    assert_eq!(
        replayed[0].watched_agent_ids,
        vec![crate::parse_agent_id("child-a")]
    );

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    assert!(h.agent_watches.is_empty());
    assert!(h.agent_watchers.is_empty());
    h.shutdown().expect("shutdown");
}

#[test]
fn disabling_agent_watch_removes_response_fanout_route() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let sender_cid = ensure_test_user_agent(&mut h);
    h.agents.get_mut(&sender_cid).expect("sender").agent_id = Some("child-agent".to_owned());
    h.agent_routes
        .insert("child-agent".to_owned(), sender_cid.clone());
    h.set_agent_watch(
        "watcher-agent",
        "child-agent",
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        "watcher-agent",
        "child-agent",
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    assert!(
        h.watchers_for_agent("child-agent").is_empty(),
        "disabled watch must remove the child from watch-response fan-out"
    );
    h.shutdown().expect("shutdown");
}

/// A watch must receive one structured initial snapshot and one start/stop pair
/// per whole model turn, while provider/tool continuations retain one
/// generation.
#[test]
fn agent_watch_reports_structured_whole_turn_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.agents.get_mut(&watcher_cid).expect("watcher").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "watcher-busy".into(),
    };

    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(
        h.agents
            .get(&watcher_cid)
            .expect("watcher")
            .pending_prompts
            .is_empty(),
        "initial state is durable client status, not a prompt for the watcher"
    );
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_turn_state(
        &watched_cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "watched-turn".into(),
        },
    );
    h.set_agent_turn_state(
        &watched_cid,
        AgentTurnState::ToolsRunning {
            remaining_calls: vec!["call-1".into()],
        },
    );
    // Internal tool-result continuation bookkeeping may temporarily use Idle,
    // but the published whole-turn state remains running.
    h.agents.get_mut(&watched_cid).expect("watched").turn_state = AgentTurnState::Idle;
    h.set_agent_turn_state(
        &watched_cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "watched-continuation".into(),
        },
    );
    h.set_agent_turn_state(&watched_cid, AgentTurnState::Idle);

    let notifications: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchTurnState)
        .collect();
    assert_eq!(notifications.len(), 3);
    let initial = notifications[0]
        .watch_turn_state
        .as_ref()
        .expect("initial payload");
    assert!(initial.initial);
    assert_eq!(initial.state, tau_proto::AgentRuntimeState::Idle);
    assert_eq!(initial.turn_generation, 0);
    let started = notifications[1]
        .watch_turn_state
        .as_ref()
        .expect("start payload");
    assert!(!started.initial);
    assert_eq!(started.state, tau_proto::AgentRuntimeState::Running);
    assert_eq!(started.turn_generation, 1);
    let stopped = notifications[2]
        .watch_turn_state
        .as_ref()
        .expect("stop payload");
    assert_eq!(stopped.state, tau_proto::AgentRuntimeState::Idle);
    assert_eq!(stopped.turn_generation, 1);
    assert_eq!(initial.subscription_id, started.subscription_id);
    assert_eq!(started.subscription_id, stopped.subscription_id);
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert_eq!(
        durable_agent_message_received_events(&h)
            .into_iter()
            .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchTurnState)
            .count(),
        3
    );

    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    h.set_agent_turn_state(
        &watched_cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "already-running-on-reenable".into(),
        },
    );
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let reenabled = session_agent_message_received_events(&h)
        .into_iter()
        .rev()
        .find(|message| message.kind == tau_proto::AgentMessageKind::WatchTurnState)
        .expect("re-enabled initial state");
    let reenabled_state = reenabled
        .watch_turn_state
        .as_ref()
        .expect("re-enabled payload");
    assert!(reenabled_state.initial);
    assert_eq!(reenabled_state.state, tau_proto::AgentRuntimeState::Running);
    assert_eq!(reenabled_state.turn_generation, 2);
    assert_ne!(reenabled_state.subscription_id, initial.subscription_id);
    h.set_agent_turn_state(&watched_cid, AgentTurnState::Idle);

    let before_notification_only_turn = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchTurnState)
        .count();
    h.agents
        .get_mut(&watched_cid)
        .expect("watched")
        .lifecycle_notification_only_turn = true;
    h.set_agent_turn_state(
        &watched_cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "notification-only-turn".into(),
        },
    );
    h.set_agent_turn_state(&watched_cid, AgentTurnState::Idle);
    let after_notification_only_turn = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchTurnState)
        .count();
    assert_eq!(
        after_notification_only_turn, before_notification_only_turn,
        "notification-only turns must not recursively fan out lifecycle state"
    );

    h.shutdown().expect("shutdown");
}

/// Mutual watches must not self-excite on lifecycle-only turns, while ordinary
/// input added during a tool continuation promotes the generation with a
/// delayed start so its eventual stop remains paired.
#[test]
fn mutual_watch_mixed_lifecycle_turn_emits_paired_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let a_cid = ensure_test_user_agent(&mut h);
    let b_cid = h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let a_id = durable_agent_id_for_conversation(&h, &a_cid).to_string();
    let b_id = durable_agent_id_for_conversation(&h, &b_cid).to_string();
    for cid in [&a_cid, &b_cid] {
        h.agents.get_mut(cid).expect("agent").turn_state = AgentTurnState::AgentThinking {
            agent_prompt_id: format!("busy-{cid}").into(),
        };
    }
    h.set_agent_watch(
        &a_id,
        &b_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        &b_id,
        &a_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.agents.get_mut(&b_cid).expect("agent b").turn_state = AgentTurnState::Idle;
    assert!(
        h.agents
            .get(&b_cid)
            .expect("agent b")
            .pending_prompts
            .is_empty(),
        "initial watch snapshots must not create lifecycle turns"
    );
    let lifecycle_prompt = PendingPrompt::watch_turn_state(
        "[tau-internal]: Watched agent peer started a model turn".to_owned(),
    );
    h.dispatch_prompt_for_agent(&b_cid, lifecycle_prompt)
        .expect("dispatch lifecycle turn");
    h.set_agent_turn_state(
        &b_cid,
        AgentTurnState::ToolsRunning {
            remaining_calls: vec!["mixed-call".into()],
        },
    );
    let fresh_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let fresh_id = durable_agent_id_for_conversation(&h, &fresh_cid).to_string();
    h.agents
        .get_mut(&fresh_cid)
        .expect("fresh watcher")
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "fresh-watcher-busy".into(),
    };
    h.set_agent_watch(
        &fresh_id,
        &b_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let before_promotion = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchTurnState
                && message.sender_id.as_str() == b_id
                && message.recipient_id.as_str() == a_id
                && !message
                    .watch_turn_state
                    .as_ref()
                    .is_some_and(|state| state.initial)
        })
        .count();
    assert_eq!(before_promotion, 0);

    h.agents
        .get_mut(&b_cid)
        .expect("agent b")
        .pending_prompts
        .push_back(PendingPrompt::internal(
            "ordinary continuation input".to_owned(),
        ));
    h.fold_pending_prompts_as_steered(&b_cid);
    h.set_agent_turn_state(&b_cid, AgentTurnState::Idle);

    let mixed_edges: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchTurnState
                && message.sender_id.as_str() == b_id
                && message.recipient_id.as_str() == a_id
                && !message
                    .watch_turn_state
                    .as_ref()
                    .is_some_and(|state| state.initial)
        })
        .collect();
    assert_eq!(mixed_edges.len(), 2);
    let start = mixed_edges[0].watch_turn_state.as_ref().expect("start");
    let stop = mixed_edges[1].watch_turn_state.as_ref().expect("stop");
    assert_eq!(start.state, tau_proto::AgentRuntimeState::Running);
    assert_eq!(stop.state, tau_proto::AgentRuntimeState::Idle);
    assert_eq!(start.turn_generation, stop.turn_generation);
    let fresh_states: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchTurnState
                && message.sender_id.as_str() == b_id
                && message.recipient_id.as_str() == fresh_id
        })
        .collect();
    assert_eq!(
        fresh_states.len(),
        2,
        "initial running plus one stop: {fresh_states:?}"
    );
    assert!(
        fresh_states[0].watch_turn_state.as_ref().is_some_and(
            |state| state.initial && state.state == tau_proto::AgentRuntimeState::Running
        )
    );
    assert!(
        fresh_states[1].watch_turn_state.as_ref().is_some_and(
            |state| !state.initial && state.state == tau_proto::AgentRuntimeState::Idle
        )
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn agent_stats_snapshots_cover_tool_and_context_transitions_and_replay() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    set_available_provider_models(&mut h, [provider_model_info("test/model".into(), 1_000)]);
    h.selected_model = Some("test/model".into());
    let stats = connect_test_tool(&mut h, "stats-live");
    h.complete_subscription(
        "stats-live",
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_stats_updated(&stats);

    let cid = ensure_test_user_agent(&mut h);
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "sp-stats".into(),
        },
    );
    h.bump_tools_started_for(&cid);
    h.update_agent_context_usage(&cid, Some(&"test/model".into()), Some(250), Some(50));
    h.tool_agents.insert("counted-call".into(), cid.clone());
    h.finish_tool_call_runtime_state("counted-call");

    let snapshots = drain_stats_updated(&stats);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.runtime_state == tau_proto::AgentRuntimeState::Running
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 1
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.context.input_tokens == Some(250)
            && snapshot.context.cached_tokens == Some(50)
            && snapshot.context.context_window == Some(1_000)
            && snapshot.context.percent_used == Some(25)
    }));

    let replay = connect_test_tool(&mut h, "stats-replay");
    h.complete_subscription(
        "stats-replay",
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
        Vec::new(),
    )
    .expect("stats replay");
    let replayed = drain_stats_updated(&replay);
    assert!(replayed.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.context.input_tokens == Some(250)
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn rejected_pre_dispatch_tool_attempt_counts_once_in_agent_stats() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let stats = connect_test_tool(&mut h, "stats-reject");
    h.complete_subscription(
        "stats-reject",
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_stats_updated(&stats);
    let cid = ensure_test_user_agent(&mut h);
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    let call = AgentToolCall {
        id: "bad-call".into(),
        name: ToolName::new("missing_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };

    h.reject_agent_tool_call_before_dispatch(
        &cid,
        &call,
        ToolName::new("missing_tool"),
        "missing tool".to_owned(),
    );

    let snapshots = drain_stats_updated(&stats);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 1
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 0
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn explicit_agent_start_role_controls_side_agent_prompt_model_and_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    set_available_provider_models(
        &mut h,
        [provider_model_info("test/role-model".into(), 8_000)],
    );
    h.system_prompt_templates.insert(
        "explicit-template".to_owned(),
        "role={{role.name}} agent={{agent_id}}".to_owned(),
    );
    h.available_roles.insert(
        "explicit-role".to_owned(),
        tau_config::settings::AgentRole {
            model: Some("test/role-model".into()),
            prompt_override: Some("explicit-template".to_owned()),
            tools: Some(vec![ToolName::new("agent_watch")]),
            effort: Some(tau_proto::Effort::High),
            ..Default::default()
        },
    );
    let _delegate = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_watch"),
            model_visible_name: None,
            description: Some("watch agent".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-explicit".to_owned(),
            instruction: "side task".to_owned(),
            role: Some("explicit-role".to_owned()),
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("explicit-call".into()),
            task_name: Some("explicit task".to_owned()),
        },
    )
    .expect("start explicit role");

    let cid = ext_query_cid(&h, "q-explicit").expect("side conversation");
    assert_eq!(
        h.agents
            .get(&cid)
            .and_then(|conversation| conversation.role.as_deref()),
        Some("explicit-role")
    );
    let spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid).then_some(spid.clone()))
        .expect("side prompt");
    let prompt = read_prompt_created(&h, &spid);
    assert_eq!(prompt.model.to_string(), "test/role-model");
    assert_eq!(prompt.model_params.effort, tau_proto::Effort::High);
    assert!(
        prompt
            .system_prompt
            .starts_with("role=explicit-role agent=")
    );
    assert_eq!(
        prompt
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["agent_watch"]
    );
    h.shutdown().expect("shutdown");
}

/// Regression: when one side conversation tears down (running
/// `snap_to_default_agent`) before another's tool result
/// arrives, the result must still fold onto the *originating*
/// conversation's branch. Before this fix, the result landed at
/// `tree.head` (which `snap_to_default` had moved to the parent
/// branch), producing orphan ToolUse blocks in subsequent prompts —
/// the exact `No tool output found for function call …` 400 we hit
/// in `tau-agent-yvxco1`'s log.
#[test]
fn sibling_side_conv_teardown_does_not_misplace_other_side_conv_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // Set up the main agent's turn that emits a single delegate call.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    // Spawn the outer side conversation.
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    // Have the outer sub-agent emit a *nested* delegate. The harness
    // should issue another StartAgentRequest for it, which we then ack
    // with a fresh side conversation. This is the exact pattern that
    // produced the misplacement: outer side conv runs teardown
    // (snap_to_default) before nested side conv's tool result lands.
    let outer_side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: outer_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");
    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    // Nested sub-agent finishes with a final answer. This triggers
    // side teardown: `snap_to_default_agent` runs, moving
    // tree.head back to the main branch. The delegate ext then
    // publishes a ToolResult for `nested-call` — which must fold on
    // the *outer* conv's branch (since outer issued nested-call), not
    // wherever tree.head happens to be.
    let nested_side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: nested_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "nested answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-nested".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("nested final");

    // The delegate extension would route the nested StartAgentResult
    // back as a ToolResult — simulate that here.
    h.handle_extension_event(
        "conn-delegate",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: "nested-call".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("nested answer".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("nested tool result");

    // Now re-prompt the outer sub-agent and inspect the assembled
    // messages. The `outer-call` tool_use must NOT appear in the
    // outer sub-agent's branch — the only ToolUse the outer
    // sub-agent should see is its own `nested-call` (with a
    // matching ToolResult).
    let outer_resume_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() == outer_side_cid_str(&h)).then_some(spid.clone())
        })
        .expect("outer resume prompt id");
    let prompt = read_prompt_created(&h, &outer_resume_spid);

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_results: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "outer sub-agent's prompt must not include the parent's `outer-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_uses.iter().any(|id| id == "nested-call"),
        "outer sub-agent's prompt must include its own `nested-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_results.iter().any(|id| id == "nested-call"),
        "outer sub-agent must see the matching ToolResult for `nested-call`; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: nested extension-agent queries must branch from the
/// conversation that issued the nested tool call. Branching from the
/// default conversation can replay unrelated in-flight ToolUse blocks
/// from the main branch into the nested sub-agent prompt, which OpenAI
/// rejects with `No tool output found for function call …`.
#[test]
fn nested_start_agent_request_branches_from_tool_owner_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let default_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_agents
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_agent(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("outer query");

    let outer_side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: outer_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    let nested_side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    let prompt = read_prompt_created(&h, &nested_side_spid);

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "nested sub-agent's prompt must not include the default branch's unresolved `outer-call`; got: {tool_uses:?}",
    );
    assert!(
        !tool_uses.iter().any(|id| id == "nested-call"),
        "nested sub-agent starts before its parent call has a result, so it must not include `nested-call`; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn completed_side_conversation_tool_result_reprompts_parent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid,
        agent_id: target_agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    let side_cid = ext_query_cid(&h, "q-outer").expect("side conversation");
    let side_agent_id = durable_agent_id_for_conversation(&h, &side_cid);
    let side_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: side_spid,
        agent_id: side_agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "outer answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side final");

    h.handle_extension_event(
        "conn-delegate",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: "outer-call".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("outer answer".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("delegate result");

    let main_resume_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid).then_some(spid.clone()))
        .expect("main resume prompt id");
    let prompt = read_prompt_created(&h, &main_resume_spid);
    let tool_results: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_results.iter().any(|id| id == "outer-call"),
        "parent conversation must be re-prompted with agent_start ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn recursive_delegate_prompt_contains_only_leaf_instruction() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let default_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = "sp-main".into();
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_agents
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_agent(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "ROOT: ask top delegate to delegate again".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "top-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-top".to_owned(),
            instruction: "TOP: delegate exactly two more subtasks".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("top-call".into()),
            task_name: Some("top".to_owned()),
        },
    )
    .expect("top query");

    let top_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("top prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: top_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "leaf-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-top".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("top response");

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: None,
            query_id: "q-leaf".to_owned(),
            instruction: "LEAF: do one terminal search only".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("leaf-call".into()),
            task_name: Some("leaf".to_owned()),
        },
    )
    .expect("leaf query");

    let leaf_spid = h
        .prompt_agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            matches!(
                h.agents
                    .get(prompt_cid)
                    .map(|conv| &conv.originator),
                Some(tau_proto::PromptOriginator::Extension { query_id, .. }) if query_id == "q-leaf"
            )
            .then_some(spid.clone())
        })
        .expect("leaf prompt id");
    let prompt = read_prompt_created(&h, &leaf_spid);
    let rendered = prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("LEAF: do one terminal search only"),
        "leaf prompt must include its own instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("TOP: delegate exactly two more subtasks"),
        "leaf prompt must not inherit parent recursive instruction; got: {rendered}",
    );
    assert!(
        !rendered.contains("ROOT: ask top delegate to delegate again"),
        "leaf prompt must not inherit ancestor task framing; got: {rendered}",
    );

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_uses.is_empty(),
        "leaf prompt must not inherit unresolved ancestor tool calls; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a delayed response for an older prompt in the same conversation
/// must not be allowed to append fresh tool calls after a newer prompt is
/// already in flight. That creates orphan `function_call` items with no
/// matching output in later full replays, which OpenAI rejects with `No tool
/// output found for function call …`.
#[test]
fn stale_same_conversation_tool_call_response_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let old_spid: AgentPromptId = "sp-old".into();
    let new_spid: AgentPromptId = "sp-new".into();
    h.prompt_agents.insert(old_spid.clone(), cid.clone());
    h.prompt_agents.insert(new_spid.clone(), cid.clone());
    {
        let conv = h.agents.get_mut(&cid).expect("default conversation");
        conv.in_flight_prompt = Some(new_spid.clone());
        conv.last_prompt_id = Some(new_spid.clone());
    }

    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: old_spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "stale-call".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
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
    })
    .expect("stale response ignored");

    assert!(
        !event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolRequest(request) if request.call_id.as_str() == "stale-call"
        )),
        "stale tool call must not be dispatched",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id.as_str() == old_spid.as_str()
                    && terminated.reason == tau_proto::AgentPromptTerminationReason::Stale
        )),
        "stale prompt must publish a terminal lifecycle event",
    );
    assert!(!h.prompt_agents.contains_key(old_spid.as_str()));
    let conv = h.agents.get(&cid).expect("default conversation");
    assert_eq!(conv.in_flight_prompt.as_ref(), Some(&new_spid));
    assert!(matches!(conv.turn_state, AgentTurnState::Idle));

    h.shutdown().expect("shutdown");
}

/// Regression: parallel side agents must not steal each
/// other's branch cursor. Before the per-event `folded_node_id`
/// sync, `commit_event` synced `c.head` from the global
/// `tree.head()`. A non-folding event on conv-A (e.g. an
/// `ProviderResponseFinished` carrying only tool calls) would overwrite
/// `c.head[conv-A]` with whatever sibling conv-B last folded — so
/// conv-A's next `ToolRequest` would graft onto conv-B's branch and
/// the resulting prompt would walk through unrelated history,
/// producing orphan ToolUse blocks the provider rejects with
/// `No tool output found for function call …`.
fn message_tool_call(id: &str, recipient_id: &str, message: &str) -> AgentToolCall {
    AgentToolCall {
        id: id.into(),
        name: ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
        tool_type: tau_proto::ToolType::Function,
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

fn session_agent_message_sent_events(h: &Harness) -> Vec<tau_proto::AgentMessageSent> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageSent(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn session_agent_message_received_events(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn durable_agent_message_sent_events(h: &Harness) -> Vec<tau_proto::AgentMessageSent> {
    loaded_agent_events(h, "s1")
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageSent(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn durable_agent_message_received_events(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    loaded_agent_events(h, "s1")
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message) => Some(message),
            _ => None,
        })
        .collect()
}

#[test]
fn message_tool_to_user_emits_only_sender_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-user", "user", "hello user"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let sent = session_agent_message_sent_events(&h);
    let received = session_agent_message_received_events(&h);
    assert_eq!(sent.len(), 1);
    let sender_agent_id = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_deref())
        .expect("sender agent id");
    assert_eq!(sent[0].sender_id.as_str(), sender_agent_id);
    assert_role_hex_agent_id(sender_agent_id, "engineer");
    assert_eq!(sent[0].recipient, tau_proto::AgentMessageRecipient::User);
    assert_eq!(sent[0].message, "hello user");
    assert!(received.is_empty());

    let durable_sent = durable_agent_message_sent_events(&h);
    let durable_received = durable_agent_message_received_events(&h);
    assert_eq!(durable_sent.len(), 1);
    assert_eq!(durable_sent[0].message_id, sent[0].message_id);
    assert_eq!(durable_sent[0].message, "hello user");
    assert!(durable_received.is_empty());

    h.shutdown().expect("shutdown");
}

/// Unknown agent recipients must fail the tool call before publishing any
/// message projection, so a typo cannot create forged transcript state.
#[test]
fn message_tool_unknown_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-bad", "missing_agent", "hello"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());
    let errors: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-bad" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unknown message recipient"));
    assert!(errors[0].message.contains("unknown"));

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must validate the active target session and then
/// publish only the harness-owned recipient projection with external sender
/// identity preserved for prompt/UI rendering.
#[test]
fn external_agent_message_request_publishes_received_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agents.get_mut(&cid).expect("conversation").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "external-message-target".into(),
    };

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-ok".to_owned(),
            message_id: "msg-external-ok".into(),
            capability: "cap-ok".to_owned(),
            sender_session_id: "other-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: "s1".into(),
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-ok");
    assert_eq!(result.error, None);
    assert!(session_agent_message_sent_events(&h).is_empty());
    let received = session_agent_message_received_events(&h);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].sender_id.as_str(), "sender_agent");
    assert_eq!(
        received[0].sender_session_id.as_deref(),
        Some("other-session")
    );
    assert_eq!(received[0].recipient_id.as_str(), recipient_id);
    assert_eq!(received[0].message, "hello from outside");
    let pending = h
        .agents
        .get(&cid)
        .expect("recipient conversation")
        .pending_prompts
        .front()
        .expect("queued external message prompt");
    assert!(pending.text.contains("other-session/sender_agent"));

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs are addressed to one active session; stale
/// runtime metadata or a racing `/session new` must not deliver into the wrong
/// active session.
#[test]
fn external_agent_message_request_rejects_wrong_active_session() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-wrong-session".to_owned(),
            message_id: "msg-external-wrong-session".into(),
            capability: "cap-wrong-session".to_owned(),
            sender_session_id: "other-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: "not-s1".into(),
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-wrong-session");
    assert!(result.error.expect("error").contains("active session"));
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must reject unknown recipients before writing a
/// durable inbound transcript projection.
#[test]
fn external_agent_message_request_rejects_unknown_recipient() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-unknown".to_owned(),
            message_id: "msg-external-unknown".into(),
            capability: "cap-unknown".to_owned(),
            sender_session_id: "other-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: "s1".into(),
            recipient_id: crate::parse_agent_id("missing_agent"),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello from outside".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-unknown");
    assert!(
        result
            .error
            .expect("error")
            .contains("unknown message recipient")
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Cross-harness message RPCs must reject empty messages before writing a
/// durable inbound transcript projection.
#[test]
fn external_agent_message_request_rejects_empty_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "external-empty".to_owned(),
            message_id: "msg-external-empty".into(),
            capability: "cap-empty".to_owned(),
            sender_session_id: "other-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: "s1".into(),
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: " \n\t ".to_owned(),
        },
    );

    assert_eq!(result.request_id, "external-empty");
    assert!(result.error.expect("error").contains("must not be empty"));
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Production external-message intake must reject invalid target-side fields
/// before starting sender authentication. Returning `Some(result)` from
/// `start_external_agent_message_auth` is the immediate-rejection path; `None`
/// means a background sender-auth helper was started.
#[test]
fn external_agent_message_auth_start_rejects_invalid_target_before_callback() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let client_id: tau_proto::ConnectionId = "external".into();

    let base = tau_proto::ExternalAgentMessageRequest {
        request_id: "external-preauth".to_owned(),
        message_id: "msg-external-preauth".into(),
        capability: "cap-preauth".to_owned(),
        sender_session_id: "other-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.current_session_id.clone(),
        recipient_id: crate::parse_agent_id(&recipient_id),
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello".to_owned(),
    };

    let cases = [
        (
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-preauth-wrong-session".to_owned(),
                recipient_session_id: "wrong-session".into(),
                ..base.clone()
            },
            "active session",
        ),
        (
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-preauth-empty".to_owned(),
                message: " \n\t ".to_owned(),
                ..base.clone()
            },
            "must not be empty",
        ),
        (
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-preauth-unknown".to_owned(),
                recipient_id: crate::parse_agent_id("missing_agent"),
                ..base
            },
            "unknown message recipient",
        ),
    ];

    for (request, expected_error) in cases {
        let request_id = request.request_id.clone();
        let result = h
            .start_external_agent_message_auth(client_id.clone(), request)
            .expect("invalid target request should be rejected immediately");
        assert_eq!(result.request_id, request_id);
        assert!(result.error.expect("error").contains(expected_error));
    }
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Sender-harness authorization must bind the bearer capability to every
/// security-sensitive field, especially the sender identity and watch-response
/// kind that the target harness will render into the recipient prompt.
#[test]
fn external_agent_message_auth_binds_sender_identity_and_kind() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let message_id: tau_proto::AgentMessageId = "msg-auth".into();
    h.pending_external_message_auth.insert(
        message_id.clone(),
        crate::harness::PendingExternalAgentMessageAuth {
            capability: "secret-capability".to_owned(),
            sender_session_id: h.current_session_id.clone(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: "target-session".into(),
            recipient_id: crate::parse_agent_id("recipient_agent"),
            kind: tau_proto::AgentMessageKind::WatchResponse,
            message: "authorized body".to_owned(),
        },
    );

    let valid = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-valid".to_owned(),
        message_id: message_id.clone(),
        capability: "secret-capability".to_owned(),
        sender_session_id: h.current_session_id.clone(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: "target-session".into(),
        recipient_id: crate::parse_agent_id("recipient_agent"),
        kind: tau_proto::AgentMessageKind::WatchResponse,
        message: "authorized body".to_owned(),
    };
    let result = h.handle_external_agent_message_auth_request(valid.clone());
    assert!(result.authorized);
    assert_eq!(result.error, None);

    let forged_kind = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-kind".to_owned(),
        kind: tau_proto::AgentMessageKind::Message,
        ..valid.clone()
    };
    let result = h.handle_external_agent_message_auth_request(forged_kind);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    let forged_sender = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-sender".to_owned(),
        sender_id: crate::parse_agent_id("attacker"),
        ..valid.clone()
    };
    let result = h.handle_external_agent_message_auth_request(forged_sender);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    let forged_body = tau_proto::ExternalAgentMessageAuthRequest {
        request_id: "auth-forged-body".to_owned(),
        message: "altered body".to_owned(),
        ..valid
    };
    let result = h.handle_external_agent_message_auth_request(forged_body);
    assert!(!result.authorized);
    assert!(result.error.expect("error").contains("does not match"));

    h.shutdown().expect("shutdown");
}

/// Generic clients and extensions must not be able to forge external-message
/// RPCs. Even the narrow external-harness hello only enables the RPC envelope;
/// sender identity and kind still require a sender-issued capability.
#[test]
fn external_agent_message_rpc_requires_external_peer_hello() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "external-forged".to_owned(),
        message_id: "msg-external-forged".into(),
        capability: "cap-forged".to_owned(),
        sender_session_id: "other-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: "s1".into(),
        recipient_id: crate::parse_agent_id(&recipient_id),
        kind: tau_proto::AgentMessageKind::Message,
        message: "forged".to_owned(),
    };

    h.handle_client_message(
        "ui",
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("untrusted client request");
    h.handle_extension_message(
        "extension",
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("extension request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.handle_client_message(
        "ui",
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "ordinary-ui".into(),
            client_kind: tau_proto::ClientKind::Ui,
        }),
    )
    .expect("ordinary hello");
    h.handle_client_message(
        "ui",
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request.clone()),
    )
    .expect("ordinary client request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.handle_client_message(
        "external",
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
            client_kind: tau_proto::ClientKind::External,
        }),
    )
    .expect("external hello");
    h.handle_client_message(
        "external",
        tau_proto::HarnessInputMessage::ExternalAgentMessage(request),
    )
    .expect("external request");
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// A real Unix-socket external client cannot deliver an external agent message
/// unless the claimed sender harness authenticates the message capability.
#[test]
fn external_agent_message_rpc_rejects_unauthenticated_socket_sender() {
    let td = TempDir::new().expect("tempdir");
    let sender_sp = td.path().join("sender-state");
    let target_sp = td.path().join("target-state");
    let mut sender = echo_harness(&sender_sp).expect("start sender");
    let mut target = echo_harness(&target_sp).expect("start target");
    let target_cid = ensure_test_user_agent(&mut target);
    let recipient_id = target
        .ensure_agent_id_for_agent(&target_cid)
        .expect("target agent id");
    target
        .agents
        .get_mut(&target_cid)
        .expect("target conversation")
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "socket-external-message-target".into(),
    };

    let socket_path = td.path().join("target.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind socket");
    let mut peer = tau_socket::SocketPeer::connect(&socket_path).expect("connect peer");
    let (stream, _) = listener.accept().expect("accept peer");
    target.accept_client(stream).expect("accept client");

    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
        client_kind: tau_proto::ClientKind::External,
    }))
    .expect("send external hello");
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessage(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "socket-external-ok".to_owned(),
            message_id: "socket-message-ok".into(),
            capability: "socket-capability".to_owned(),
            sender_session_id: sender.current_session_id.clone(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: target.current_session_id.clone(),
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello over socket".to_owned(),
        },
    ))
    .expect("send external message");

    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let result = loop {
        match target.rx.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(HarnessEvent::FromConnection {
                connection_id,
                message,
            }) => {
                target
                    .handle_client_message(&connection_id, *message)
                    .expect("handle socket message");
            }
            Ok(HarnessEvent::Command(command)) => target
                .handle_harness_command(command)
                .expect("handle auth completion"),
            Ok(other) => target.log_event(&other),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("target harness event channel disconnected");
            }
        }
        match peer
            .recv_timeout(std::time::Duration::from_millis(20))
            .expect("receive rpc result")
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(result),
            } => break result,
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout if Instant::now() < deadline => continue,
            tau_socket::SocketReceive::Timeout => panic!("timed out waiting for rpc result"),
            tau_socket::SocketReceive::Closed => panic!("socket closed before rpc result"),
        }
    };

    assert_eq!(result.request_id, "socket-external-ok");
    assert!(
        result
            .error
            .expect("authentication error")
            .contains("no running daemon for sender session")
    );
    assert!(session_agent_message_received_events(&target).is_empty());

    target.shutdown().expect("shutdown target");
    sender.shutdown().expect("shutdown sender");
}

/// Receiver-side sender authentication must run off the central harness loop.
/// A request whose claimed sender cannot authenticate should enqueue a helper
/// and return promptly instead of blocking every other harness event until the
/// socket/auth timeout path completes.
#[test]
fn external_agent_message_authentication_starts_without_blocking_client_handler() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");

    h.handle_client_message(
        "external",
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
            client_kind: tau_proto::ClientKind::External,
        }),
    )
    .expect("external hello");

    let started = Instant::now();
    h.handle_client_message(
        "external",
        tau_proto::HarnessInputMessage::ExternalAgentMessage(
            tau_proto::ExternalAgentMessageRequest {
                request_id: "external-nonblocking-auth".to_owned(),
                message_id: "msg-external-nonblocking-auth".into(),
                capability: "cap-nonblocking".to_owned(),
                sender_session_id: "missing-sender-session".into(),
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient_session_id: h.current_session_id.clone(),
                recipient_id: crate::parse_agent_id(&recipient_id),
                kind: tau_proto::AgentMessageKind::Message,
                message: "hello".to_owned(),
            },
        ),
    )
    .expect("external request");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "external auth should be delegated off-loop"
    );
    assert!(session_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Sender-side result waiting must outlast receiver-side sender authentication.
/// The target cannot send `external_agent_message_result` until its auth helper
/// finishes, so equal deadlines can make the sender close the socket exactly
/// when the receiver is ready to report an auth timeout or late success.
#[test]
fn external_agent_message_result_timeout_outlasts_auth_timeout() {
    let auth_timeout = crate::harness::subagents_tool::EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT;
    let result_timeout = crate::harness::subagents_tool::EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT;

    assert!(
        auth_timeout < result_timeout,
        "result timeout must leave room for target-side auth completion"
    );
}

/// Sender-side external message projections should represent confirmed
/// delivery, not a failed lookup or target-side rejection.
#[test]
fn external_message_send_failure_does_not_publish_sent_projection() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: tau_proto::ToolCallId = "external-message-call".into();
    h.tool_agents.insert(call_id.clone(), cid.clone());

    h.publish_external_agent_message_from_agent(
        &cid,
        "missing-session".into(),
        crate::parse_agent_id("recipient_agent"),
        "hello".to_owned(),
        tau_proto::AgentMessageKind::Message,
        Some(
            crate::harness::subagents_tool::ExternalMessageToolCompletion {
                conversation_id: cid.clone(),
                session_generation: h.current_session_generation,
                call_id: call_id.clone(),
                tool_name: ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
                tool_type: tau_proto::ToolType::Function,
                details: CborValue::Null,
            },
        ),
    )
    .expect("start external send");
    assert!(session_agent_message_sent_events(&h).is_empty());
    assert_eq!(h.pending_external_message_auth.len(), 1);

    let command = loop {
        match h
            .rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("completion command")
        {
            HarnessEvent::Command(command) => break command,
            other => h.log_event(&other),
        }
    };
    h.handle_harness_command(command)
        .expect("handle completion");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(h.pending_external_message_auth.is_empty());
    assert!(
        event_log_events(&h)
            .into_iter()
            .any(|event| { matches!(event, Event::ToolError(error) if error.call_id == call_id) })
    );

    h.shutdown().expect("shutdown");
}

/// A successful asynchronous external-message completion publishes the deferred
/// sender projection and completes the original tool call.
#[test]
fn external_message_success_completion_publishes_sent_projection_and_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: tau_proto::ToolCallId = "external-message-success-call".into();
    h.tool_agents.insert(call_id.clone(), cid.clone());

    h.handle_harness_command(crate::event::HarnessCommand::ExternalMessageToolCompleted(
        Box::new(crate::event::ExternalMessageToolCompletedCommand {
            conversation_id: cid,
            session_generation: h.current_session_generation,
            call_id: call_id.clone(),
            tool_name: ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            result: Ok(()),
            details: CborValue::Null,
            auth_message_id: "delivered-message".into(),
            sent_event: Some(tau_proto::AgentMessageSent {
                message_id: "delivered-message".into(),
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
                    session_id: "other-session".into(),
                    agent_id: crate::parse_agent_id("recipient_agent"),
                },
                kind: tau_proto::AgentMessageKind::Message,
                message: "delivered".to_owned(),
            }),
        }),
    ))
    .expect("handle completion");

    assert_eq!(session_agent_message_sent_events(&h).len(), 1);
    assert!(
        event_log_events(&h)
            .into_iter()
            .any(|event| matches!(event, Event::ToolResult(result) if result.call_id == call_id))
    );

    h.shutdown().expect("shutdown");
}

/// Stale async external-message completions from an abandoned session must not
/// publish sender projections or tool completions after `/session new`, even if
/// the new session happens to reinstall the same conversation/tool ownership
/// keys before the old worker finishes.
#[test]
fn stale_external_message_completion_after_session_switch_with_reused_ids_is_dropped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call_id: tau_proto::ToolCallId = "stale-external-message-call".into();
    h.tool_agents.insert(call_id.clone(), cid.clone());

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    let replacement_cid = ensure_test_user_agent(&mut h);
    let replacement_agent = h
        .agents
        .remove(&replacement_cid)
        .expect("replacement agent");
    h.agents.insert(cid.clone(), replacement_agent);
    h.tool_agents.insert(call_id.clone(), cid.clone());

    h.handle_harness_command(crate::event::HarnessCommand::ExternalMessageToolCompleted(
        Box::new(crate::event::ExternalMessageToolCompletedCommand {
            conversation_id: cid,
            session_generation: 0,
            call_id: call_id.clone(),
            tool_name: ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            result: Ok(()),
            details: CborValue::Null,
            auth_message_id: "stale-message".into(),
            sent_event: Some(tau_proto::AgentMessageSent {
                message_id: "stale-message".into(),
                sender_id: crate::parse_agent_id("sender_agent"),
                recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
                    session_id: "other-session".into(),
                    agent_id: crate::parse_agent_id("recipient_agent"),
                },
                kind: tau_proto::AgentMessageKind::Message,
                message: "stale".to_owned(),
            }),
        }),
    ))
    .expect("handle stale completion");

    let events = event_log_events(&h);
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentMessageSent(message) if message.message_id.as_str() == "stale-message"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolResult(result) if result.call_id == call_id
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ToolError(error) if error.call_id == call_id
    )));

    h.shutdown().expect("shutdown");
}

/// `/session new` reuses the daemon process, so the daemon's runtime discovery
/// metadata must advertise the new active session and stop matching the stale
/// old session after `switch_session` succeeds.
#[test]
fn switch_session_updates_runtime_metadata_active_session() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let harness_path = td.path().join("runtime-harness");
    std::fs::write(
        crate::runtime_dir::metadata_path(&harness_path),
        serde_json::to_vec(&crate::runtime_dir::DaemonMetadata {
            version: 1,
            pid: std::process::id(),
            project_root: Some(td.path().to_path_buf()),
            session_id: "s1".to_owned(),
        })
        .expect("metadata json"),
    )
    .expect("write metadata");
    h.set_runtime_harness_path(harness_path.clone());

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert_eq!(
        crate::runtime_dir::read_session_id(&harness_path).as_deref(),
        Some("s2")
    );

    h.shutdown().expect("shutdown");
}

/// Runtime metadata update is discovery-only and must not leave the harness
/// half-switched if the metadata file is missing or unreadable.
#[test]
fn switch_session_continues_when_runtime_metadata_update_fails() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.set_runtime_harness_path(td.path().join("missing-runtime-harness"));

    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("switch session should continue after metadata update failure");

    assert_eq!(h.current_session_id.as_str(), "s2");
    assert!(event_log_contains_any_source(&h, |event| {
        matches!(event, Event::SessionStarted(started) if started.session_id.as_str() == "s2")
    }));

    h.shutdown().expect("shutdown");
}

/// A completed agent used to be collapsed with a typo as an unknown recipient.
/// Keep the error distinct so callers can decide whether to retry or fix the
/// id.
#[test]
fn message_tool_stopped_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let stopped_cid: AgentId = crate::parse_agent_id("stopped-recipient");
    h.agents.insert(
        stopped_cid.clone(),
        Agent::new(
            stopped_cid.clone(),
            "s1".into(),
            tau_proto::PromptOriginator::User,
            None,
            None,
        ),
    );
    let recipient_id = h.ensure_agent_id_for_agent(&stopped_cid).expect("agent id");
    h.remove_agent(&stopped_cid);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-stopped", &recipient_id, "hello"),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());
    let errors: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-stopped" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("stopped message recipient"));
    assert!(errors[0].message.contains("stopped"));

    h.shutdown().expect("shutdown");
}

/// Agent-directed messages are displayed in the UI like every message
/// projection, and the recipient agent receives an internal queued prompt with
/// stable markup.
#[test]
fn message_tool_to_agent_queues_internal_prompt_markup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agents.get_mut(&cid).expect("conversation").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-message-target".into(),
    };

    h.handle_message_tool_call(
        &cid,
        &message_tool_call(
            "msg-agent",
            &recipient_id,
            "secret <message>&</message> payload >",
        ),
        ToolName::new(crate::harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let sent = session_agent_message_sent_events(&h);
    let received = session_agent_message_received_events(&h);
    assert_eq!(sent.len(), 1);
    assert_eq!(received.len(), 1);
    assert_eq!(sent[0].message_id, received[0].message_id);
    assert_eq!(sent[0].sender_id.as_str(), recipient_id);
    assert_eq!(received[0].sender_id.as_str(), recipient_id);
    assert_eq!(received[0].recipient_id.as_str(), recipient_id);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::Message);

    let durable_sent = durable_agent_message_sent_events(&h);
    let durable_received = durable_agent_message_received_events(&h);
    assert_eq!(durable_sent.len(), 1);
    assert_eq!(durable_received.len(), 1);
    assert_eq!(durable_sent[0].message_id, sent[0].message_id);
    assert_eq!(durable_received[0].message_id, received[0].message_id);

    let conv = h.agents.get(&cid).expect("conversation");
    let queued = conv.pending_prompts.back().expect("queued prompt");
    assert_eq!(
        queued.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert!(queued.text.contains(&format!(
        "[tau-internal]: You have received a message from {recipient_id}"
    )));
    assert!(queued.text.contains(
        "<message>\nsecret &lt;message&gt;&amp;&lt;/message&gt; payload &gt;\n</message>"
    ));

    h.shutdown().expect("shutdown");
}

/// Watch response notifications must not look like explicit `message` tool
/// deliveries in the model-visible prompt; otherwise the receiving agent cannot
/// tell whether a sub-agent finished a watched turn or intentionally messaged
/// it.
#[test]
fn agent_watch_response_queues_distinct_internal_prompt_markup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agents.get_mut(&cid).expect("conversation").turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-watch-target".into(),
    };

    h.publish_agent_watch_response_from_agent(
        &cid,
        recipient_id.clone(),
        "done <response>&</response> payload >".to_owned(),
    )
    .expect("watch response");

    assert!(session_agent_message_sent_events(&h).is_empty());
    let received = session_agent_message_received_events(&h);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchResponse);

    let conv = h.agents.get(&cid).expect("conversation");
    let queued = conv.pending_prompts.back().expect("queued prompt");
    assert_eq!(
        queued.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert!(queued.text.contains(&format!(
        "[tau-internal]: Watched agent {recipient_id} emitted a response"
    )));
    assert!(queued.text.contains(
        "<response>\ndone &lt;response&gt;&amp;&lt;/response&gt; payload &gt;\n</response>"
    ));
    assert!(!queued.text.contains("You have received a message"));
    assert!(!queued.text.contains("<message>"));

    h.shutdown().expect("shutdown");
}

/// A watcher must be told when the watched agent accepts a direct user prompt,
/// otherwise the watched agent's next response can look like an unsolicited
/// reply to the watcher instead of a response to fresh user input.
#[test]
fn user_prompt_to_watched_agent_notifies_watchers_with_prompt_markup() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = h.agents[&watched_cid]
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agents[&watcher_cid]
        .agent_id
        .clone()
        .expect("watcher agent id");
    h.pending_agent_context_ready
        .remove(&tau_proto::AgentId::parse(&watcher_id).expect("watcher id"));
    h.agents
        .get_mut(&watcher_cid)
        .expect("watcher conversation")
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-watcher-busy".into(),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.handle_ui_prompt_submitted(UiPromptSubmitted {
        session_id: h.current_session_id.clone(),
        text: "please continue <now>&</now> >".to_owned(),
        agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
    .expect("prompt submitted");

    assert!(session_agent_message_sent_events(&h).is_empty());
    let received: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt)
        .collect();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchPrompt);
    assert_eq!(received[0].sender_id, crate::parse_agent_id(&watched_id));
    assert_eq!(received[0].recipient_id, crate::parse_agent_id(&watcher_id));

    let watcher = h.agents.get(&watcher_cid).expect("watcher conversation");
    let queued = watcher
        .pending_prompts
        .iter()
        .find(|prompt| prompt.text.contains("received a user prompt"))
        .expect("queued prompt notification");
    assert_eq!(
        queued.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert!(queued.text.contains(&format!(
        "[tau-internal]: Watched agent {watched_id} received a user prompt"
    )));
    assert!(
        queued
            .text
            .contains("<prompt>\nplease continue &lt;now&gt;&amp;&lt;/now&gt; &gt;\n</prompt>")
    );
    assert!(!queued.text.contains("finished its turn"));
    assert!(!queued.text.contains("<response>"));

    h.shutdown().expect("shutdown");
}

/// Internal prompts delivered to a watched agent, including background tool
/// completion notices and steering prompts, must not be reflected as
/// `agent_watch` prompt notifications. Only user-visible prompts are watchable
/// context for the watcher.
#[test]
fn internal_prompt_to_watched_agent_does_not_notify_watchers() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = h.agents[&watched_cid]
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agents[&watcher_cid]
        .agent_id
        .clone()
        .expect("watcher agent id");
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.handle_ui_prompt_submitted(UiPromptSubmitted {
        session_id: h.current_session_id.clone(),
        text: "[tau-internal] Tool call `call-1` is complete.".to_owned(),
        agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
    .expect("internal prompt submitted");

    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt),
        "internal prompts to watched agents must not be forwarded to watchers"
    );

    h.shutdown().expect("shutdown");
}

/// A queued prompt must not notify watchers until it becomes the watched
/// agent's active turn. Otherwise the watcher can receive "prompt arrived" and
/// then the previous turn's response, which is exactly the ordering ambiguity
/// watch prompt notifications are meant to remove.
#[test]
fn queued_user_prompt_notifies_watchers_when_dispatched_not_when_queued() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = h.agents[&watched_cid]
        .agent_id
        .clone()
        .expect("watched agent id");
    let watcher_id = h.agents[&watcher_cid]
        .agent_id
        .clone()
        .expect("watcher agent id");
    h.pending_agent_context_ready
        .remove(&tau_proto::AgentId::parse(&watcher_id).expect("watcher id"));
    h.agents
        .get_mut(&watched_cid)
        .expect("watched conversation")
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-watched-current".into(),
    };
    h.agents
        .get_mut(&watcher_cid)
        .expect("watcher conversation")
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: "sp-watcher-busy".into(),
    };
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.handle_ui_prompt_submitted(UiPromptSubmitted {
        session_id: h.current_session_id.clone(),
        text: "queued follow-up".to_owned(),
        agent_id: tau_proto::AgentId::parse(&watched_id).expect("watched id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
    .expect("prompt queued");

    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt),
        "queued prompt must not notify watchers before it becomes active"
    );

    h.agents
        .get_mut(&watched_cid)
        .expect("watched conversation")
        .turn_state = AgentTurnState::Idle;
    h.try_advance_queue();

    let received: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchPrompt)
        .collect();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::WatchPrompt);
    assert_eq!(received[0].message, "queued follow-up");

    h.shutdown().expect("shutdown");
}

/// Agent ids are minted once per conversation as role-prefixed hex strings and
/// are removed from the reverse lookup when the conversation is torn down.
#[test]
fn agent_id_generation_is_stable_and_cleaned_up() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    let first = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let second = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    assert_eq!(first, second);
    assert_role_hex_agent_id(&first, "engineer");
    assert_eq!(h.agent_routes.get(&first), Some(&cid));

    h.remove_agent(&cid);
    assert!(!h.agent_routes.contains_key(&first));

    h.shutdown().expect("shutdown");
}

/// External clients and extensions must not forge message projection events;
/// only the harness-owned message tool may publish them.
#[test]
fn inbound_agent_message_events_are_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let forged_sent = Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: "test-message".into(),
        sender_id: crate::parse_agent_id("attacker"),
        recipient: tau_proto::AgentMessageRecipient::User,
        kind: tau_proto::AgentMessageKind::Message,
        message: "forged".to_owned(),
    });
    let forged_received = Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: "test-message-received".into(),
        sender_id: crate::parse_agent_id("attacker"),
        sender_session_id: Some("other-session".into()),
        recipient_id: crate::parse_agent_id("victim"),
        kind: tau_proto::AgentMessageKind::Message,
        watch_turn_state: None,
        message: "forged received".to_owned(),
    });
    for forged in [forged_sent, forged_received] {
        h.handle_client_event_inner("ui", forged.clone())
            .expect("client event");
        h.handle_extension_event_inner("extension", forged.clone())
            .expect("extension event");
        h.handle_extension_message(
            "extension",
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(forged),
                transient: false,
            }),
        )
        .expect("extension emit");
    }

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());

    h.shutdown().expect("shutdown");
}

/// Extensions must not forge harness-owned or otherwise non-extension-owned
/// facts through the generic fallback `emit` path.
#[test]
fn inbound_non_extension_owned_fallback_events_are_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    for forged in [
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "forged-session".into(),
            reason: tau_proto::SessionStartReason::New,
        }),
        Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "forged-session".into(),
        }),
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "forged-session".into(),
            agent_id: crate::parse_agent_id("forged-agent"),
            ephemeral: false,
        }),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "forged-session".into(),
            agent_id: crate::parse_agent_id("forged-agent"),
        }),
        Event::AgentStarted(tau_proto::AgentStarted {
            parent_agent: None,
            agent_id: crate::parse_agent_id("forged-agent"),
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
        Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            query_id: "delegate-0".to_owned(),
            agent_id: crate::parse_agent_id("forged-agent"),
        }),
        Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: "delegate-0".to_owned(),
            text: "forged result".to_owned(),
            error: None,
        }),
        Event::ToolDelegateProgress(tau_proto::DelegateProgress {
            call_id: "delegate-call".into(),
            task_name: "forged task".to_owned(),
            agent_id: Some(crate::parse_agent_id("forged-agent")),
            role: Some("engineer".to_owned()),
            ctx_percent: None,
            ctx_input_tokens: None,
            ctx_window: None,
            tools_in_flight: 0,
            tools_total: 0,
            display: None,
        }),
        Event::ProviderCacheMissDiagnostic(tau_proto::ProviderCacheMissDiagnostic {
            agent_prompt_id: "forged-prompt".into(),
            model: "provider/model".into(),
            originator: tau_proto::PromptOriginator::User,
            tool_choice: tau_proto::ToolChoice::default(),
            ws_pool_delta: None,
            input_tokens: 1,
            cached_tokens: 0,
            previous_input_tokens: 1,
            cacheable_input_tokens: 1,
            corrected_cache_efficiency: 0.0,
        }),
    ] {
        let baseline_seq = h.event_log.next_seq();
        h.handle_extension_message(
            "extension",
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(forged.clone()),
                transient: false,
            }),
        )
        .expect("extension emit");
        assert!(
            h.event_log.get_next_from(baseline_seq).is_none(),
            "forged {} must not be published",
            forged.name()
        );
    }

    assert!(h.store.session("forged-session").is_none());
    assert!(
        h.agent_store
            .agent_events("forged-agent")
            .expect("agent events")
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

fn cache_miss_diagnostic_for_test(prompt_id: &str) -> tau_proto::ProviderCacheMissDiagnostic {
    tau_proto::ProviderCacheMissDiagnostic {
        agent_prompt_id: prompt_id.into(),
        model: "provider/model".into(),
        originator: tau_proto::PromptOriginator::User,
        tool_choice: tau_proto::ToolChoice::default(),
        ws_pool_delta: None,
        input_tokens: 1,
        cached_tokens: 0,
        previous_input_tokens: 1,
        cacheable_input_tokens: 1,
        corrected_cache_efficiency: 0.0,
    }
}

/// Provider diagnostics must come from the provider that owns the prompt route.
#[test]
fn provider_cache_miss_diagnostic_requires_prompt_owner() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    connect_test_client(&mut h, "provider-a", tau_proto::ClientKind::Provider);
    connect_test_client(&mut h, "provider-b", tau_proto::ClientKind::Provider);
    h.pending_provider_prompts
        .insert("prompt-1".into(), "provider-a".into());

    let baseline_seq = h.event_log.next_seq();
    h.handle_extension_message(
        "provider-b",
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderCacheMissDiagnostic(
                cache_miss_diagnostic_for_test("prompt-1"),
            )),
            transient: false,
        }),
    )
    .expect("non-owner diagnostic emit");
    assert!(h.event_log.get_next_from(baseline_seq).is_none());

    h.handle_extension_message(
        "provider-a",
        TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ProviderCacheMissDiagnostic(
                cache_miss_diagnostic_for_test("prompt-1"),
            )),
            transient: false,
        }),
    )
    .expect("owner diagnostic emit");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderCacheMissDiagnostic(diagnostic)
            if diagnostic.agent_prompt_id.as_str() == "prompt-1"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_group_overrides_apply_before_individual_tool_overrides() {
    // Role group toggles are coarse-grained defaults. Individual tool toggles
    // must run after them so a role can enable a whole group and exclude one
    // dangerous tool, or disable a group and keep one explicit exception.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.available_roles.insert(
        h.selected_role.clone(),
        tau_config::settings::AgentRole {
            enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
            disable_tools: vec![ToolName::new("email_trash")],
            disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
            enable_tools: vec![ToolName::new("shell_safe")],
            ..Default::default()
        },
    );

    for (name, group) in [
        ("email_read", "pim"),
        ("email_trash", "pim"),
        ("shell_exec", "shell"),
        ("shell_safe", "shell"),
    ] {
        h.registry.register_with_prompt_fragment(
            "conn-grouped",
            tau_proto::ToolRegister {
                tool: ToolSpec {
                    name: ToolName::new(name),
                    model_visible_name: None,
                    description: Some(name.to_owned()),
                    tool_type: tau_proto::ToolType::Function,
                    parameters: None,
                    format: None,
                    tags: Vec::new(),
                    enabled_by_default: false,
                    background_support: None,
                    examples: Vec::new(),
                },
                tool_group: Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new(group),
                    prompt_fragment: (name == "email_trash").then(|| {
                        tau_proto::PromptFragment::new(
                            format!("{group}.instructions"),
                            tau_proto::PromptPriority::new(10),
                            format!("{group} GROUP PROMPT"),
                        )
                    }),
                }),
                prompt_fragment: None,
            },
        );
    }

    let defs = h.gather_tool_definitions_for_role(&h.selected_role);
    let names = defs.iter().map(|def| def.name.as_str()).collect::<Vec<_>>();
    assert!(
        names.contains(&"email_read"),
        "expected group-enabled tool: {names:?}"
    );
    assert!(
        !names.contains(&"email_trash"),
        "individual disable must win: {names:?}"
    );
    assert!(
        !names.contains(&"shell_exec"),
        "group disable should hide tool: {names:?}"
    );
    assert!(
        names.contains(&"shell_safe"),
        "individual enable must win: {names:?}"
    );
    let prompt_fragments = h.gather_prompt_fragments();
    let pim_group_prompts = prompt_fragments
        .iter()
        .filter(|fragment| fragment.template.as_str() == "pim GROUP PROMPT")
        .count();
    assert_eq!(pim_group_prompts, 1, "group prompt renders once");
}

/// Extension prompt submission is the sanctioned intake path for side-effect
/// bridges like Telegram. It must produce the same durable user prompt fact as
/// a UI prompt instead of accepting forged `agent.prompt_submitted` events.
#[test]
fn extension_prompt_submit_request_routes_to_loaded_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.handle_extension_event(
        "telegram-ext",
        TestProtocolItem::Event(Event::ExtPromptSubmitRequest(
            tau_proto::ExtPromptSubmitRequest {
                agent_id: agent_id.clone(),
                text: "[telegram from alice] hello".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                ctx_id: Some("telegram-1".to_owned()),
            },
        )),
    )
    .expect("submit prompt request");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.agent_id == agent_id
                && prompt.text == "[telegram from alice] hello"
                && prompt.message_class == tau_proto::PromptMessageClass::User
                && prompt.originator.is_user()
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id == agent_id && created.ctx_id.as_deref() == Some("telegram-1")
    )));

    h.shutdown().expect("shutdown");
}

/// Internal extension prompt submissions must stay hidden while still producing
/// harness-owned prompt facts for timer and other wakeup extensions.
#[test]
fn internal_extension_prompt_submit_request_routes_as_internal_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.handle_extension_event(
        "utils-ext",
        TestProtocolItem::Event(Event::ExtPromptSubmitRequest(
            tau_proto::ExtPromptSubmitRequest {
                agent_id: agent_id.clone(),
                text: "timer fired".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
                ctx_id: Some("timer:wake:1".to_owned()),
            },
        )),
    )
    .expect("submit internal prompt request");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.agent_id == agent_id
                && prompt.text == "timer fired"
                && prompt.message_class == tau_proto::PromptMessageClass::Internal
                && prompt.ctx_id.as_deref() == Some("timer:wake:1")
    )));

    h.shutdown().expect("shutdown");
}

/// Bad extension prompt targets must be rejected with user-visible harness
/// notice and must not create durable prompt facts for arbitrary agent ids.
#[test]
fn extension_prompt_submit_request_rejects_unknown_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");

    h.handle_extension_event(
        "telegram-ext",
        TestProtocolItem::Event(Event::ExtPromptSubmitRequest(
            tau_proto::ExtPromptSubmitRequest {
                agent_id: tau_proto::AgentId::parse("missing-agent").expect("agent id"),
                text: "hello".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                ctx_id: None,
            },
        )),
    )
    .expect("reject prompt request");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(info) if info.message.contains("unknown or unloaded agent")
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "hello"
    )));

    h.shutdown().expect("shutdown");
}

/// Queued extension prompt-submit requests preserve their ctx_id when they are
/// folded as steering messages, giving timer restore replayable fired evidence.
#[test]
fn queued_extension_prompt_submit_request_preserves_ctx_id_when_steered() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "busy-prompt".into(),
        },
    );
    h.handle_extension_event(
        "utils-ext",
        TestProtocolItem::Event(Event::ExtPromptSubmitRequest(
            tau_proto::ExtPromptSubmitRequest {
                agent_id: agent_id.clone(),
                text: "timer fired".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
                ctx_id: Some("timer:wake:1".to_owned()),
            },
        )),
    )
    .expect("queue internal prompt request");
    h.fold_pending_prompts_as_steered(&cid);

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.agent_id == agent_id
                && steered.text == "timer fired"
                && steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.ctx_id.as_deref() == Some("timer:wake:1")
    )));

    h.shutdown().expect("shutdown");
}

/// Queued extension prompt-submit requests must keep their own ctx_id values;
/// a single per-agent scratch slot would make later queued prompts overwrite
/// earlier request correlation ids.
#[test]
fn queued_extension_prompt_submit_requests_preserve_individual_ctx_ids() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: "busy-prompt".into(),
        },
    );

    for (text, ctx_id) in [("first", "ctx-1"), ("second", "ctx-2")] {
        h.handle_extension_event(
            "telegram-ext",
            TestProtocolItem::Event(Event::ExtPromptSubmitRequest(
                tau_proto::ExtPromptSubmitRequest {
                    agent_id: agent_id.clone(),
                    text: text.to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    ctx_id: Some(ctx_id.to_owned()),
                },
            )),
        )
        .expect("queue prompt request");
    }

    let queued_ctx_ids: Vec<_> = h.agents[&cid]
        .pending_prompts
        .iter()
        .map(|prompt| prompt.ctx_id.as_deref())
        .collect();
    assert_eq!(queued_ctx_ids, vec![Some("ctx-1"), Some("ctx-2")]);

    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    h.try_advance_queue();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id == agent_id && created.ctx_id.as_deref() == Some("ctx-1")
    )));

    if let Some(agent) = h.agents.get_mut(&cid) {
        agent.in_flight_prompt = None;
    }
    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    h.try_advance_queue();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id == agent_id && created.ctx_id.as_deref() == Some("ctx-2")
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn agent_metadata_validation_rejects_bad_key_size_value_and_unknown_target() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let agent_id = tau_proto::AgentId::parse("metadata-target").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());

    let valid = tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("ok"),
        value: CborValue::Text("value".to_owned()),
        inheritable: false,
    };
    h.validate_agent_metadata_set(&valid)
        .expect("valid metadata set");

    let empty_key = tau_proto::AgentMetadataSet {
        key: tau_proto::AgentMetadataKey::new(""),
        ..valid.clone()
    };
    assert!(
        h.validate_agent_metadata_set(&empty_key)
            .expect_err("empty key rejected")
            .contains("must not be empty")
    );

    let oversized_key = tau_proto::AgentMetadataSet {
        key: tau_proto::AgentMetadataKey::new(
            "k".repeat(tau_proto::MAX_AGENT_METADATA_KEY_BYTES + 1),
        ),
        ..valid.clone()
    };
    assert!(
        h.validate_agent_metadata_set(&oversized_key)
            .expect_err("oversized key rejected")
            .contains("exceeds 256 bytes")
    );

    let oversized_value = tau_proto::AgentMetadataSet {
        value: CborValue::Bytes(vec![0; tau_proto::MAX_AGENT_METADATA_VALUE_BYTES + 1]),
        ..valid.clone()
    };
    assert!(
        h.validate_agent_metadata_set(&oversized_value)
            .expect_err("oversized value rejected")
            .contains("exceeds 64 KiB")
    );

    let unknown = tau_proto::AgentMetadataSet {
        agent_id: tau_proto::AgentId::parse("unknown-agent").expect("agent id"),
        ..valid
    };
    assert!(
        h.validate_agent_metadata_set(&unknown)
            .expect_err("unknown target rejected")
            .contains("unknown agent metadata target")
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn explicit_parent_agent_start_inherits_only_inheritable_metadata() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    h.submit_user_prompt("s1".into(), "parent prompt".to_owned())
        .expect("submit parent");
    let parent_agent_id = h
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.agent_id.clone())
        .expect("parent agent id");
    let parent = tau_proto::AgentId::parse(&parent_agent_id).expect("parent agent id");
    let inherit_key = tau_proto::AgentMetadataKey::new("inherit-key");
    let local_key = tau_proto::AgentMetadataKey::new("local-key");

    for (key, value, inheritable) in [
        (inherit_key.clone(), "inherited", true),
        (local_key, "local", false),
    ] {
        h.publish_event(
            None,
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: parent.clone(),
                key,
                value: CborValue::Text(value.to_owned()),
                inheritable,
            }),
        );
    }

    h.handle_start_agent_request(
        "conn-delegate",
        StartAgentRequest {
            parent_agent: Some(parent.clone()),
            query_id: "q-inherit".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start child");
    let child_cid = ext_query_cid(&h, "q-inherit").expect("child conversation");
    let child_agent_id = h
        .agents
        .get(&child_cid)
        .and_then(|conversation| conversation.agent_id.clone())
        .expect("child agent id");

    let child_events = h
        .agent_store
        .agent_events(&child_agent_id)
        .expect("child events");
    assert!(child_events.iter().any(|entry| matches!(
        &entry.event,
        Event::AgentMetadataSet(set)
            if set.agent_id.as_str() == child_agent_id
                && set.key == inherit_key
                && set.value == CborValue::Text("inherited".to_owned())
                && set.inheritable
    )));
    assert!(child_events.iter().all(|entry| !matches!(
        &entry.event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "local-key"
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures full prompt rendering with AGENTS disabled returns only the system
/// wrapper and never falls back to harness-side filesystem discovery.
#[test]
fn rendered_prompt_without_agents_md_uses_system_wrapper_only() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);

    let result = request_rendered_prompt(&mut h, "debug-role", false);
    let prompt = result.prompt.expect("rendered prompt");

    assert_eq!(result.error, None);
    assert!(prompt.contains("<message role=\"system\">"));
    assert!(prompt.contains("DEBUG ROLE PROMPT"));
    assert!(prompt.contains("Your agent id is `dev-preview-agent`."));
    assert!(!prompt.contains("source=\"AGENTS.md\""));
    assert!(!prompt.contains("AGENTS_FILE"));
}

/// Ensures full prompt rendering uses only harness-owned discovered AGENTS.md
/// state when AGENTS inclusion is enabled.
#[test]
fn rendered_prompt_with_seeded_agents_md_includes_synthetic_agents_message() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: "shell".into(),
        file_path: td.path().join("AGENTS.md"),
        content: "seeded AGENTS instructions\n".to_owned(),
    });

    let result = request_rendered_prompt(&mut h, "debug-role", true);
    let prompt = result.prompt.expect("rendered prompt");

    assert_eq!(result.error, None);
    assert!(prompt.contains("<message role=\"user\" synthetic=\"true\" source=\"AGENTS.md\">"));
    assert!(prompt.contains("<AGENTS_FILE path="));
    assert!(prompt.contains("seeded AGENTS instructions"));
}

/// Ensures full prompt rendering reports unknown roles in-band on the request
/// path instead of synthesizing a prompt.
#[test]
fn rendered_prompt_unknown_role_returns_in_band_error() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    seed_render_prompt_role(&mut h);

    let result = request_rendered_prompt(&mut h, "missing-role", true);

    assert_eq!(result.prompt, None);
    assert_eq!(result.error, Some("unknown role: missing-role".to_owned()));
}

fn seed_render_prompt_role(h: &mut Harness) {
    h.available_roles = std::collections::HashMap::from([(
        "debug-role".to_owned(),
        tau_config::settings::AgentRole {
            prompt_fragments: vec![tau_config::settings::RolePromptFragment {
                name: "debug.instructions".to_owned(),
                priority: tau_proto::PromptPriority::new(100),
                text: tau_proto::PromptContent::new("DEBUG ROLE PROMPT"),
            }],
            ..Default::default()
        },
    )]);
}

fn request_rendered_prompt(
    h: &mut Harness,
    role: &str,
    enable_agents_md: bool,
) -> tau_proto::RenderedPromptResult {
    let frames = connect_test_client(h, "render-prompt-test", tau_proto::ClientKind::Ui);
    h.send_rendered_prompt_result(
        "render-prompt-test",
        tau_proto::GetRenderedPrompt {
            request_id: "request-1".to_owned(),
            role: role.to_owned(),
            enable_agents_md,
        },
    );
    let frames = frames.lock().expect("frames lock");
    frames
        .iter()
        .find_map(|frame| match &frame.frame {
            HarnessOutputMessage::RenderedPromptResult(result) => Some((**result).clone()),
            _ => None,
        })
        .expect("rendered prompt result")
}

#[test]
fn ui_emitted_custom_event_routes_to_subscribed_extension() {
    // A UI client drives a stateful extension (the task factory's
    // request/snapshot exchange) by emitting an extension-owned custom event.
    // It must reach an extension that subscribed to that event name, the same
    // way an extension-emitted custom event reaches the UI.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    let ext = connect_test_client(&mut h, "factory-ext", tau_proto::ClientKind::Tool);
    let command_name: tau_proto::EventName = "factory.sync".parse().expect("event name");
    h.bus
        .set_subscriptions(
            "factory-ext",
            Vec::new(),
            vec![EventSelector::Prefix("factory.".to_owned())],
        )
        .expect("extension subscription");
    let _ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);

    h.handle_client_event_inner(
        "ui",
        Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(command_name.clone(), None, CborValue::Null)
                .expect("valid custom event"),
        ),
    )
    .expect("ui custom event handled");

    let frames = ext.lock().expect("extension sink");
    assert!(frames.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ExtensionEvent(custom)) if *custom.name() == command_name
    )));
}

#[test]
fn ui_cannot_emit_custom_event_with_reserved_first_party_category() {
    // The reserved-category guard lives in `CustomEvent::try_new`, which the
    // deserialize path re-runs — so a UI cannot construct a custom event named
    // under `harness`/`agent`/`tool`/`extension` to spoof a first-party fact.
    let reserved: tau_proto::EventName = "harness.notice".parse().expect("event name");
    assert!(
        tau_proto::CustomEvent::try_new(reserved, None, CborValue::Null).is_err(),
        "reserved first-party category must be rejected at construction"
    );
}
