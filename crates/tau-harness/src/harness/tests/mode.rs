use super::*;
use crate::{
    event as path_crate_event, event_log as path_crate_event_log, harness as path_crate_harness,
};

fn wait_for_socket(sock: &Path) {
    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3), "socket timeout");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ensures embedded mode returns provider output and persists the resulting
/// history/debug events.
#[test]
fn embedded_mode_returns_provider_response_and_persists_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "hello")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "response should not be empty: {r:?}");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let branch = persisted_agent_branch(&sp, "s1");
    assert!(
        2 <= branch.len(),
        "should have user msg + agent response, got {}",
        branch.len()
    );

    // Debug-log mirror: every turn that goes through the harness
    // should produce both a committed report line and a canonical `published`
    // line capturing the enriched copy the harness committed. This is what
    // cache/cost-analysis tooling reads.
    let jsonl = std::fs::read_to_string(sessions_dir.join("s1").join("events.jsonl"))
        .expect("events.jsonl should exist for session s1");
    let parsed: Vec<serde_json::Value> = jsonl
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl"))
        .collect();
    let reported_finished = parsed
        .iter()
        .filter(|e| {
            e["type"] == "published" && e["event_name"] == "provider.response_finished_reported"
        })
        .count();
    let published_finished = parsed
        .iter()
        .filter(|e| e["type"] == "published" && e["event_name"] == "provider.response_finished")
        .count();
    assert!(
        1 <= reported_finished,
        "expected ≥1 committed provider.response_finished_reported line, got {reported_finished}",
    );
    assert!(
        1 <= published_finished,
        "expected ≥1 published provider.response_finished line, got {published_finished}",
    );
}

/// Guards the `--ephemeral` persistence boundary: the harness must not create
/// per-session metadata, locks, debug traces, or stderr log directories, while
/// the current product decision keeps agent transcripts durable.
#[test]
fn ephemeral_daemon_suppresses_session_artifacts_but_keeps_agents() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions {
                    max_clients: Some(1),
                    storage_mode: crate::HarnessStorageMode::SessionEphemeral,
                    ..Default::default()
                },
            )
        }
    });

    wait_for_socket(&sock);

    let response = send_daemon_message(&sock, "s1", "hello").expect("prompt");
    assert_eq!(response, "hello");

    server.join().expect("join").expect("daemon clean exit");

    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    assert!(
        !sessions_dir.join("s1").exists(),
        "ephemeral session must not create a per-session state directory"
    );

    let agent_store = AgentStore::open(sp.join("agents")).expect("agent store");
    let persisted_nodes: usize = agent_store
        .agents()
        .into_iter()
        .map(|agent| agent.nodes().len())
        .sum();
    assert!(
        2 <= persisted_nodes,
        "agent transcripts remain persistent in session-ephemeral mode"
    );
}

/// Documents the live protocol contract for session-ephemeral harnesses:
/// subscribers see an explicit ephemeral marker instead of a real session path.
#[test]
fn ephemeral_harness_reports_display_only_session_dir() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = quiet_provider_harness_ephemeral(&sp).expect("harness");

    let session_dirs: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessSessionDir(session_dir) => Some(session_dir),
            _ => None,
        })
        .collect();
    assert!(
        session_dirs.iter().any(|session_dir| {
            session_dir.session_id == "s1"
                && session_dir.status == tau_proto::SessionDirStatus::Ephemeral
                && session_dir.path.as_os_str() == "<ephemeral>"
        }),
        "ephemeral harness must announce a display-only session dir marker: {session_dirs:?}"
    );
}

/// Prevents session-scoped extension data from punching a persistence hole in
/// `--ephemeral`: the request is rejected before helpers can create
/// `<state>/sessions/<session_id>/ext/data/...`.
#[test]
fn ephemeral_harness_rejects_session_scoped_extension_data() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = quiet_provider_harness_ephemeral(&sp).expect("harness");
    let provider_connection = h
        .extension_connection_id("provider")
        .expect("provider connection")
        .to_owned();

    let error = h
        .run_extension_data_request(
            &crate::test_connection_id(&provider_connection),
            tau_proto::ExtensionDataScope::Session,
            tau_proto::ExtensionDataRequestOp::WriteFile {
                path: tau_proto::ExtensionDataPath::from("notes.txt"),
                contents: b"secret-ish session data".to_vec(),
            },
        )
        .expect_err("session-scoped extension data should be unavailable");

    assert_eq!(error.kind, tau_proto::ExtensionDataErrorKind::Permission);
    assert!(
        error.message.contains("ephemeral"),
        "error should explain the ephemeral boundary: {}",
        error.message
    );
    assert!(
        !tau_config::settings::sessions_dir_of(&sp)
            .join("s1")
            .exists(),
        "rejected session-scoped extension data must not create a session directory"
    );
}

/// Memory-only mode rejects every delegated extension-data scope before path
/// resolution and leaves the complete supplied state tree absent.
#[test]
fn memory_only_harness_rejects_all_extension_data_without_state_roots() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = quiet_provider_harness_with_start_reason_and_storage_mode(
        &sp,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::MemoryOnly,
    )
    .expect("memory-only harness");
    let provider_connection = h
        .extension_connection_id("provider")
        .expect("provider connection")
        .to_owned();

    for scope in [
        tau_proto::ExtensionDataScope::Session,
        tau_proto::ExtensionDataScope::User,
        tau_proto::ExtensionDataScope::Cache,
    ] {
        let error = h
            .run_extension_data_request(
                &crate::test_connection_id(&provider_connection),
                scope,
                tau_proto::ExtensionDataRequestOp::WriteFile {
                    path: tau_proto::ExtensionDataPath::from("notes.txt"),
                    contents: b"must remain memory-only".to_vec(),
                },
            )
            .expect_err("all persistent extension data should be unavailable");
        assert_eq!(error.kind, tau_proto::ExtensionDataErrorKind::Permission);
    }

    assert!(
        !sp.exists(),
        "memory-only construction and denied extension data must not create state roots"
    );
}

/// Keeps the harness API aligned with the CLI: an ephemeral launch must start a
/// fresh runtime-only session instead of claiming to resume durable session
/// state that it deliberately does not load or update.
#[test]
fn ephemeral_harness_rejects_resume_launch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let error = match quiet_provider_harness_with_start_reason_and_storage_mode(
        &sp,
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::SessionEphemeral,
    ) {
        Ok(mut harness) => {
            let _ = harness.shutdown();
            panic!("ephemeral resume should be rejected");
        }
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("cannot resume"),
        "error should explain the invalid launch mode: {error}"
    );
}

/// Guards per-agent ephemerality in an otherwise durable session: creation
/// facts must be live/replayable from memory while neither the agent transcript
/// nor the session membership fact is written under Tau state.
#[test]
fn ephemeral_agent_uses_memory_only_agent_and_membership_stores() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create ephemeral agent");

    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started),
            _ => None,
        })
        .expect("ephemeral agent.started");
    let agent_id = started.agent_id;
    assert!(
        event_log_events(&h).into_iter().any(|event| matches!(
            event,
            Event::SessionAgentLoaded(loaded)
                if loaded.agent_id == agent_id && loaded.ephemeral
        )),
        "ephemeral session.agent_loaded should be announced live"
    );
    assert!(
        !sp.join("agents").join(agent_id.as_str()).exists(),
        "ephemeral agent must not create an agent directory"
    );
    assert!(
        h.store
            .session_events("s1")
            .expect("durable session events")
            .into_iter()
            .all(|record| match record.event {
                Event::SessionAgentLoaded(loaded) => loaded.agent_id != agent_id,
                Event::SessionAgentUnloaded(unloaded) => unloaded.agent_id != agent_id,
                _ => true,
            }),
        "ephemeral agent membership must not be durable"
    );
    assert!(
        !h.agent_store
            .agent_events(agent_id.as_str())
            .expect("memory replay events")
            .is_empty(),
        "ephemeral agent creation should be replayable while daemon lives"
    );
}

/// Prevents durable debug JSONL from becoming a parallel transcript for
/// ephemeral agent creation, prompt, message, tool, and provider traffic,
/// including forged identities and late duplicate reports.
#[test]
fn ephemeral_agent_traffic_is_suppressed_from_debug_log() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    let request = tau_proto::UiCreateAgent {
        request_id: "test-create-request".to_owned(),
        literal: false,
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        role: "engineer".to_owned(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: Some("debug-log-secret".to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: Some("ephemeral-debug-log-prompt".to_owned()),
        parent_agent: None,
        ephemeral: true,
    };

    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("ui-test"),
        tau_proto::HarnessInputMessage::emit(Event::UiCreateAgent(request)),
    ));
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create ephemeral agent");
    let agent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral agent");
    let cid = h
        .agent_routes
        .get(agent_id.as_str())
        .cloned()
        .expect("ephemeral route");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptQueued(tau_proto::AgentPromptQueued {
            agent_id: agent_id.clone(),
            text: "published-debug-secret".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            agent_prompt_id: "ephemeral-debug-prompt"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id.clone(),
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("ephemeral-debug-ctx".to_owned()),
        }),
    );
    let tool_call_id = ToolCallId::from("ephemeral-debug-tool-call");
    h.tool_agents.insert(tool_call_id.clone(), cid.clone());
    let progress_owner = "ephemeral-progress-owner";
    let _progress_sink = connect_ready_configured_extension(
        &mut h,
        progress_owner,
        "configured-progress-owner",
        tau_proto::ClientKind::Tool,
    );
    h.pending_tool_providers.insert(
        tool_call_id.clone(),
        crate::test_connection_id(progress_owner),
    );
    let progress_report = Event::ToolProgressReported(tau_proto::ToolProgress {
        call_id: tool_call_id.clone(),
        tool_name: ToolName::new("debug_secret_tool"),
        message: Some("tool-progress-debug-secret".to_owned()),
        progress: None,
        display: None,
    });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(progress_owner),
        tau_proto::HarnessInputMessage::emit_with_persist(progress_report.clone(), false),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(progress_owner), progress_report)
        .expect("commit ephemeral tool progress report");
    let terminal_report = Event::ToolResultReported(ToolResult {
        call_id: tool_call_id,
        tool_name: ToolName::new("debug_secret_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("tool-result-debug-secret".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(progress_owner),
        tau_proto::HarnessInputMessage::emit_with_persist(terminal_report.clone(), false),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(progress_owner), terminal_report)
        .expect("commit ephemeral terminal tool report");
    let message_fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("configured-bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("ephemeral-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "message-debug-secret",
    ));
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id("bridge-connection"),
        tau_proto::HarnessInputMessage::emit(message_fact.clone()),
    ));
    h.commit_message_fact(
        Some(&crate::test_connection_id("bridge-connection")),
        message_fact,
    );

    let provider = "ephemeral-provider";
    connect_ready_configured_extension(&mut h, provider, provider, tau_proto::ClientKind::Provider);
    let provider_prompt_id: AgentPromptId = "ephemeral-provider-prompt"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    seed_agent_thinking(&mut h, &cid, provider_prompt_id.as_str());
    h.prompt_agents
        .insert(provider_prompt_id.clone(), cid.clone());
    h.agents
        .get_mut(&cid)
        .expect("ephemeral agent")
        .in_flight_prompt = Some(provider_prompt_id.clone());
    h.agents
        .get_mut(&cid)
        .expect("ephemeral agent")
        .last_prompt_id = Some(provider_prompt_id.clone());
    h.pending_provider_prompts.insert(
        provider_prompt_id.clone(),
        crate::test_connection_id(provider),
    );
    connect_test_client(&mut h, "retry-requester", tau_proto::ClientKind::Ui);
    h.handle_client_event_inner(
        &crate::test_connection_id("retry-requester"),
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse("ephemeral-ui-retry")
                .expect("retry id"),
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: Some(agent_id.clone()),
            agent_prompt_id: None,
        }),
    )
    .expect("request ephemeral retry");
    let retry_request_id = h
        .pending_retry_prompts
        .keys()
        .next()
        .cloned()
        .expect("provider retry correlation");
    let retry_debug_id = retry_request_id.as_str().to_owned();
    let submitted = Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
        agent_prompt_id: provider_prompt_id.clone(),
        originator: tau_proto::PromptOriginator::User,
    });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(submitted.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), submitted)
        .expect("commit ephemeral submitted report");
    let update = Event::ProviderResponseUpdatedReported(tau_proto::ProviderResponseUpdated {
        agent_prompt_id: provider_prompt_id.clone(),
        agent_id: tau_proto::AgentId::parse("forged-durable-agent").expect("agent id"),
        deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "ephemeral-provider-update-secret".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(update.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), update)
        .expect("commit ephemeral update report");
    let cache =
        Event::ProviderCacheMissDiagnosticReported(tau_proto::ProviderCacheMissDiagnostic {
            agent_prompt_id: provider_prompt_id.clone(),
            model: "ephemeral-provider/cache-secret".into(),
            originator: tau_proto::PromptOriginator::User,
            tool_choice: tau_proto::ToolChoice::default(),
            ws_pool_delta: None,
            input_tokens: 1,
            cached_tokens: 0,
            previous_input_tokens: 1,
            cacheable_input_tokens: 1,
            corrected_cache_efficiency: 0.0,
        });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(cache.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), cache)
        .expect("commit ephemeral cache report");
    let retry_result =
        Event::ProviderRetryPromptResultReported(tau_proto::ProviderRetryPromptResult {
            request_id: retry_request_id,
            agent_prompt_id: provider_prompt_id.clone(),
            status: tau_proto::RetryPromptStatus::Accepted,
        });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(retry_result.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), retry_result.clone())
        .expect("commit ephemeral retry report");
    let finished =
        Event::ProviderResponseFinishedReported(super::dispatch::provider_text_response(
            &provider_prompt_id,
            tau_proto::AgentId::parse("forged-durable-agent").expect("agent id"),
            "ephemeral-provider-finished-secret",
        ));
    h.remove_agent(&cid);
    assert!(!h.agents.contains_key(&cid));
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(finished.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), finished.clone())
        .expect("commit ephemeral finished report");
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(finished.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), finished)
        .expect("commit late ephemeral finished report");
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(provider),
        tau_proto::HarnessInputMessage::emit_transient(retry_result.clone()),
    ));
    h.handle_extension_event_inner(&crate::test_connection_id(provider), retry_result)
        .expect("commit duplicate ephemeral retry report");
    let duplicate_terminal_report = Event::ToolResultReported(ToolResult {
        call_id: ToolCallId::from("ephemeral-debug-tool-call"),
        tool_name: ToolName::new("debug_secret_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("duplicate-terminal-debug-secret".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    h.log_event(&path_crate_event::HarnessEvent::from_connection_for_test(
        crate::test_connection_id(progress_owner),
        tau_proto::HarnessInputMessage::emit_with_persist(duplicate_terminal_report.clone(), false),
    ));
    h.handle_extension_event_inner(
        &crate::test_connection_id(progress_owner),
        duplicate_terminal_report,
    )
    .expect("commit duplicate terminal report after ephemeral agent removal");

    let jsonl = std::fs::read_to_string(
        tau_config::settings::sessions_dir_of(&sp)
            .join("s1")
            .join("events.jsonl"),
    )
    .expect("durable session debug log exists");
    assert!(
        !jsonl.contains("debug-log-secret"),
        "ephemeral create-agent prompt must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("published-debug-secret"),
        "published ephemeral agent content must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("agent.prompt_started") && !jsonl.contains("ephemeral-debug-prompt"),
        "ephemeral prompt lifecycle metadata must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("tool-result-debug-secret")
            && !jsonl.contains("duplicate-terminal-debug-secret")
            && !jsonl.contains("tool.result_reported")
            && !jsonl.contains("\"tool.result\"")
            && !jsonl.contains("provider.tool_result"),
        "raw, committed, and canonical ephemeral terminal results must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("tool-progress-debug-secret")
            && !jsonl.contains("tool.progress_reported")
            && !jsonl.contains("\"tool.progress\""),
        "raw, committed, and canonical ephemeral tool progress must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("message-debug-secret") && !jsonl.contains("ephemeral-message"),
        "raw and committed ephemeral message facts must not be mirrored into debug JSONL"
    );
    assert!(
        !jsonl.contains("ephemeral-provider-update-secret")
            && !jsonl.contains("ephemeral-provider-finished-secret")
            && !jsonl.contains("ephemeral-provider/cache-secret")
            && !jsonl.contains("ephemeral-provider-prompt")
            && !jsonl.contains("provider.prompt_submitted_reported")
            && !jsonl.contains("provider.response_updated_reported")
            && !jsonl.contains("provider.response_finished_reported")
            && !jsonl.contains("provider.retry_prompt_result_reported")
            && !jsonl.contains(&retry_debug_id),
        "raw, committed, canonical, forged-identity, and late provider execution traffic for an ephemeral agent must not enter debug JSONL:\n{jsonl}"
    );
}

/// Guards the debug-log classifier for delegate requests: when an ephemeral
/// agent's in-flight tool starts a side agent, the request still targets the
/// ephemeral branch even without an explicit parent id.
#[test]
fn tool_backed_start_agent_request_targets_ephemeral_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create ephemeral agent");
    let agent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral agent");
    let cid = h
        .agent_routes
        .get(agent_id.as_str())
        .cloned()
        .expect("ephemeral route");
    let tool_call_id = ToolCallId::from("ephemeral-delegate-tool-call");
    h.tool_agents.insert(tool_call_id.clone(), cid);

    assert!(
        h.event_targets_ephemeral_agent(
            &Event::StartAgentRequest(StartAgentRequest {
                query_id: "ephemeral-tool-delegate".to_owned(),
                instruction: "delegate without leaking prompt text".to_owned(),
                role: Some("engineer".to_owned()),
                input_stats: tau_proto::ToolUseStats::default(),
                tool_call_id: Some(tool_call_id),
                task_name: Some("ephemeral delegate".to_owned()),
                parent_agent: None,
            }),
            None,
        ),
        "tool-backed delegate requests from ephemeral agents must be classified as ephemeral"
    );
}

/// Ensures terminal tool-event debug-log classification can still identify an
/// ephemeral owner from the publish-stamped conversation snapshot after
/// call-id tracking has been cleared while interception delayed commit.
#[test]
fn sync_head_classifies_ephemeral_terminal_tool_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create ephemeral agent");
    let agent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral agent");
    let cid = h
        .agent_routes
        .get(agent_id.as_str())
        .cloned()
        .expect("ephemeral route");
    let sync = path_crate_harness::interception::ConversationHeadSync {
        cid,
        agent_id: Some(agent_id),
        session_generation: h.current_session_generation,
        suppress_activation_dispatch: false,
        continuation: None,
        notify_watchers: false,
    };

    for event in [
        Event::ToolResult(ToolResult {
            call_id: ToolCallId::from("cleared-before-commit-tool"),
            tool_name: ToolName::new("debug_secret_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("tool-result-debug-secret".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Event::ProviderToolResult(ToolResult {
            call_id: ToolCallId::from("cleared-before-commit"),
            tool_name: ToolName::new("debug_secret_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("tool-result-debug-secret".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    ] {
        assert!(
            h.event_targets_ephemeral_agent(&event, Some(&sync)),
            "{} must use sync_head_for after call tracking is cleared",
            event.name()
        );
    }
}

/// Prevents delegated work from leaking an ephemeral parent's task into a
/// durable child transcript: children inherit the parent's memory-only policy
/// unless the parent is durable.
#[test]
fn ephemeral_parent_start_agent_request_creates_ephemeral_child() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create ephemeral parent");
    let parent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral parent");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        tau_proto::StartAgentRequest {
            query_id: "q-ephemeral-child".to_owned(),
            instruction: "delegate without durable transcript".to_owned(),
            role: Some("engineer".to_owned()),
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: Some(parent_id),
        },
    )
    .expect("start child");

    let child = event_log_events(&h)
        .into_iter()
        .rev()
        .filter_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .next()
        .expect("ephemeral child");
    assert!(
        !sp.join("agents").join(child.as_str()).exists(),
        "child of ephemeral parent must not create durable agent state"
    );
}

/// UI-created agents with an ephemeral parent inherit memory-only persistence;
/// default-false `ui.create_agent.ephemeral` is not an opt-out switch.
#[test]
fn ui_create_agent_inherits_ephemeral_parent_persistence() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("harness");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("create parent");
    let parent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("parent");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: Some(parent_id),
            ephemeral: false,
        },
    )
    .expect("create child");

    let ephemeral_started = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::AgentStarted(started) if started.ephemeral))
        .count();
    assert_eq!(
        ephemeral_started, 2,
        "UI child of ephemeral parent must also be ephemeral"
    );
}

/// Ensures daemon mode accepts multiple later socket clients and persists both
/// cycles.
#[test]
fn daemon_mode_accepts_later_clients() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(2).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let r1 = send_daemon_message(&sock, "s1", "hello").expect("first");
    let r2 = send_daemon_message(&sock, "s1", "again").expect("second");
    assert_eq!(r1, "hello", "first cycle should echo our submission");
    assert_eq!(r2, "again", "second cycle should echo our submission");

    server.join().expect("join").expect("daemon clean exit");
    let branches = persisted_agent_branches(&sp, "s1");
    // The sandbox may not have any AGENTS.md to inject, so assert the
    // two user-visible cycles rather than an environment-dependent total.
    let mut submitted_user_texts: Vec<&str> = branches
        .iter()
        .flat_map(|branch| branch.iter())
        .filter_map(|entry| match entry {
            AgentEntry::UserInput { items, .. } => items.iter().find_map(|item| match item {
                ContextItem::Message(message) if message.role == ContextRole::User => {
                    message.content.first().map(|part| match part {
                        ContentPart::Text { text } => text.as_str(),
                    })
                }
                _ => None,
            }),
            _ => None,
        })
        .filter(|text| *text == "hello" || *text == "again")
        .collect();
    submitted_user_texts.sort_unstable();
    assert_eq!(
        submitted_user_texts,
        vec!["again", "hello"],
        "expected both submitted prompts to persist, got {branches:?}"
    );
    assert_eq!(
        branches
            .iter()
            .flat_map(|branch| branch.iter())
            .filter(|entry| matches!(entry, AgentEntry::ToolResults { .. }))
            .count(),
        2,
        "expected both tool result rounds to persist, got {branches:?}"
    );
}

/// Ensures daemon debug system-prompt rendering uses the requested role over
/// the socket path.
#[test]
fn daemon_mode_renders_system_prompt_for_requested_role() {
    // `tau dev print-system-prompt` asks the daemon for the rendered system
    // prompt. Exercise the socket helper rather than a direct Harness call so
    // the debug command's request/response path is covered.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let prompt = get_daemon_rendered_system_prompt(&sock, "engineer").expect("render prompt");
    assert!(!prompt.contains("## Your mission"));
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "# Your identity")
            .count(),
        1
    );
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "# Tau harness")
            .count(),
        1
    );
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "# Agent identity")
            .count(),
        1
    );
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "## Best practices")
            .count(),
        1
    );
    assert_eq!(
        prompt
            .lines()
            .filter(|line| *line == "### Best practices")
            .count(),
        0
    );
    assert!(prompt.contains("Your agent id is `dev-preview-agent`."));

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon debug tool rendering uses the requested role over the socket
/// path.
#[test]
fn daemon_mode_renders_tool_definitions_for_requested_role() {
    // `tau dev print-tools` asks the daemon for the same tool definitions the
    // harness would include in provider prompts. Cover the socket endpoint so
    // role filtering stays shared with actual agent turns.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let tools =
        get_daemon_rendered_tool_definitions(&sock, "engineer").expect("render tool definitions");
    assert!(!tools.is_empty());
    let read_tool = tools
        .iter()
        .find(|tool| tool.name.as_str() == "read")
        .expect("read tool should be available");
    assert!(
        read_tool
            .description
            .as_deref()
            .is_some_and(|d| d.contains("Reads a file"))
    );
    assert!(read_tool.parameters.is_some());

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon tool rendering reports unknown roles instead of using
/// fallback data.
#[test]
fn daemon_mode_reports_unknown_role_for_rendered_tool_definitions_request() {
    // Tool diagnostics should fail in-band for role typos, matching prompt
    // diagnostics and avoiding a misleading dump for the selected fallback role.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let error =
        get_daemon_rendered_tool_definitions(&sock, "missing-role").expect_err("unknown role");
    assert!(
        matches!(error, HarnessError::Participant(message) if message.contains("unknown role"))
    );

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon prompt rendering reports unknown roles instead of using
/// fallback data.
#[test]
fn daemon_mode_reports_unknown_role_for_rendered_system_prompt_request() {
    // The debug prompt endpoint must fail in-band with a participant error for
    // typos, instead of silently falling back to the selected role and printing
    // misleading prompt content.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let error = get_daemon_rendered_system_prompt(&sock, "missing-role").expect_err("unknown role");
    assert!(
        matches!(error, HarnessError::Participant(message) if message.contains("unknown role"))
    );

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures embedded mode can execute the read tool against a real file fixture.
#[test]
fn embedded_mode_can_read_files() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let fp = td.path().join("note.txt");
    std::fs::write(&fp, "hello from disk").expect("write fixture");
    let r = run_embedded_message_with_echo(&sp, "s1", &format!("read {}", fp.display()))
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "read response should not be empty");
    assert!(r.contains("hello from disk"));
}

/// Ensures embedded mode can execute shell commands through the echo harness.
#[test]
fn embedded_mode_can_run_shell_commands() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "shell printf hi")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "shell response should not be empty");
}

/// Ensures a full embedded shell round observes committed canonical progress
/// and traces the correlated call/result without provider bytes.
#[test]
fn traced_embedded_observes_canonical_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
    assert!(!o.response.is_empty(), "shell response should not be empty");
    assert!(!o.progress_messages.is_empty());
    assert_eq!(o.tool_calls.len(), 1);
    assert_eq!(o.tool_calls[0].name.as_str(), "shell");
    assert_eq!(o.tool_results.len(), 1);
    assert_eq!(o.tool_results[0].call_id, o.tool_calls[0].call_id);
    assert!(o.tool_results[0].provider_content.is_empty());
}

/// Ensures an embedded deterministic echo-tool round preserves exact provider
/// lifecycle cardinality and naturally clears every harness in-flight prompt.
#[test]
fn embedded_deterministic_tool_round_clears_prompt_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start embedded harness");
    let outcome = harness
        .send_user_message("s1", "quota lifecycle fixture", None)
        .expect("complete deterministic tool round");
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].name.as_str(), "echo");
    assert_eq!(outcome.tool_results.len(), 1);
    assert_eq!(
        outcome.tool_results[0].call_id, outcome.tool_calls[0].call_id,
        "the continuation must carry the exact deterministic tool result"
    );

    let events = event_log_events(&harness);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ProviderPromptSubmitted(_)))
            .count(),
        2,
        "initial tool call and tool-result continuation submit once each"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
            .count(),
        2,
        "both logical prompts reach exactly one terminal"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ToolResult(_)))
            .count(),
        1,
        "the no-side-effect echo tool executes exactly once"
    );
    assert!(
        harness
            .agents
            .values()
            .all(|agent| agent.in_flight_prompt.is_none()),
        "committed continuation terminal must clear harness in-flight state"
    );
    harness.shutdown().expect("shutdown embedded harness");
}

/// Ensures daemon-mode shell interactions report lifecycle events and clean up
/// their owned socket path after the daemon exits.
#[test]
fn traced_daemon_reports_lifecycle_and_cleans_up_socket_for_shell_run() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let o = send_daemon_message_with_trace(&sock, "s1", "shell printf hi").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell ready")
    );
    // Socket clients may miss short-lived progress if the shell command
    // completes before the writer drains the transient event.
    assert!(!o.response.is_empty(), "shell response should not be empty");
    server.join().expect("join").expect("clean exit");
    assert!(!sock.exists(), "daemon socket should be cleaned up");
}

/// Ensures traced embedded runs report provider lifecycle messages.
#[test]
fn traced_embedded_reports_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "hello").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider starting")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider exited")
    );
}

/// Ensures daemon helpers surface an in-band socket disconnect reason as a
/// participant error.
#[test]
fn daemon_disconnect_reason_is_reported() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let listener = bind_listener(&sock).expect("bind");

    let server = thread::spawn(move || {
        let mut accepted = listener.accept().expect("accept");
        let _ = accepted.recv(); // hello
        let _ = accepted.recv(); // subscribe
        let _ = accepted.recv(); // message
        accepted
            .send(&HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some("test disconnect".to_owned()),
            }))
            .expect("write");
    });

    let err =
        send_daemon_message_with_trace(&sock, "s1", "hello").expect_err("should get disconnect");
    assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
    server.join().expect("join");
}

/// Ensures harness startup eagerly initializes the configured session before
/// use.
#[test]
fn harness_startup_eagerly_initializes_eager_session() {
    // Guards against the recurring "this looks like redundant work"
    // urge to lazy-ify session init. `echo_harness` calls
    // `Harness::new_with_provider`, which must eagerly initialize the
    // session before returning — see the design-choice comment in
    // the constructor for why.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");

    assert!(
        h.initialized_sessions.contains("s1"),
        "eager init should mark the bound session as initialized at startup; \
         `initialized_sessions` was {:?}",
        h.initialized_sessions
    );
    assert!(
        matches!(h.turn_state, TurnState::Idle),
        "turn state should be Idle after eager init completes"
    );
}

/// Ensures resumed startup publishes a resume-flavored SessionStarted event.
#[test]
fn resumed_startup_publishes_resume_session_started() {
    // Restored daemons get only the eager startup `SessionStarted` to tell
    // extensions that existing per-session state should be resumed instead of
    // treated as a brand-new harness session.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("start");

    let mut next_seq = path_crate_event_log::EventLogSeq::new(0);
    let mut session_started_reason = None;
    while let Some(entry) = h.event_log.get_next_from(next_seq) {
        next_seq = entry.seq.next();
        if let Event::SessionStarted(started) = entry.event
            && started.session_id.as_str() == "s1"
        {
            session_started_reason = Some(started.reason);
            break;
        }
    }

    assert_eq!(
        session_started_reason,
        Some(tau_proto::SessionStartReason::Resume)
    );
    h.shutdown().expect("shutdown");
}
