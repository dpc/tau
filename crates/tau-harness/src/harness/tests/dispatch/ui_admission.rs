//! Tests for ui admission behavior.

use super::super::super::ui_shell_provider_ids;
use super::*;

/// Live navigation must restore the same selected-branch provider usage that a
/// cold restart derives, including after leaving and reselecting the branch.
#[test]
fn navigation_reconciles_usage_from_selected_branch_response() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("measured branch".to_owned()))
        .expect("dispatch measured branch");
    let prompt = read_nth_prompt_created(&h, 0);
    let mut response = provider_text_response(
        &prompt.agent_prompt_id,
        prompt.agent_id,
        "measured response",
    );
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some("test/model".into()),
        prompt_sent_tokens: 900,
        prompt_cached_tokens: 450,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 10,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("finish measured response");
    let measured_head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("measured response head");

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(measured_head),
        }),
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        Some(900)
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        Some(450)
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model
            .as_ref(),
        Some(&tau_proto::ModelId::from("test/model"))
    );
    h.shutdown().expect("shutdown");
}

/// Prompt-anchor reconstruction consumes exact persisted sequences, including
/// raw message facts, and rewinds to the prompt node's exact durable parent.
#[test]
fn ui_tree_prompt_anchor_preserves_raw_message_fact_parent_sequence() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = true;
    h.commit_message_fact(
        None,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("bridge")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("tree-message-fact"),
            tau_proto::MessageParty {
                stable_id: "external-user".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "raw message fact before prompt",
        )),
    );
    let message_fact_node = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("raw message fact node");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = false;
    append_user_message_via_event(&mut h, "s1", "first prompt");
    let first_prompt_node = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("first prompt node");
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &test_agent_prompt_id("sp-tree-anchor"),
            agent_id.clone(),
            "assistant answer",
        )),
    );
    let assistant_node = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("assistant node");
    assert_ne!(first_prompt_node, assistant_node);

    h.handle_ui_navigate_tree(
        &crate::test_connection_id("ui"),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(1),
        },
    )
    .expect("navigate to first prompt anchor");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(message_fact_node),
        "anchor 1 rewinds to the exact durable parent before the first prompt"
    );

    h.handle_ui_navigate_tree(
        &crate::test_connection_id("ui"),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::Node(assistant_node),
        },
    )
    .expect("navigate to raw assistant node explicitly");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(assistant_node)
    );
}

/// `UiCreateAgent.metadata` must be embedded in the durable creation fact so
/// replay restores shell cwd before `session.agent_loaded`.
#[test]
fn ui_create_agent_embeds_shell_cwd_metadata_in_agent_started() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cwd = path_std_path::PathBuf::from("/tmp/tau-ui-cwd");

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            parent_agent: None,
            session_id: test_session_id("s1"),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: vec![tau_proto::AgentInitialMetadata {
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text(cwd.display().to_string()),
                inheritable: true,
            }],
            initial_prompt: Some("hello from create".to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("create-cwd-prompt".to_owned()),
            ephemeral: false,
        },
    )
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
    let created_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find_map(|agent| agent.identity.agent_id.as_deref())
        .expect("created agent id");
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(created_id)
            .expect("created journal")
            .iter()
            .filter(|record| matches!(record.event, Event::AgentUserInteractionRecorded(_)))
            .count(),
        1,
        "accepted immediate initial prompt records exactly one interaction fact"
    );

    h.shutdown().expect("shutdown");
}

/// UI shell routing rejects zero and multiple execution owners before target
/// creation/delivery, while one owner accepts point-to-point execution.
#[test]
fn ui_shell_routing_enforces_exactly_one_provider_at_event_boundary() {
    fn command(agent_id: tau_proto::AgentId, id: &str) -> tau_proto::UiShellCommand {
        tau_proto::UiShellCommand {
            session_id: test_session_id("s1"),
            command_id: test_shell_command_id(id),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id),
        }
    }

    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    let mut stale = command(agent_id.clone(), "stale");
    stale.session_id = test_session_id("old-session");
    h.handle_ui_shell_command(&crate::test_connection_id("ui"), stale);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(
                |event| matches!(event, Event::ShellCommandFinished(finished)
            if finished.command_id.as_str() == "stale"
                && finished.output.contains("stale session"))
            )
            .count(),
        1
    );
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        command(agent_id.clone(), &crate::test_connection_id("one")),
    );
    assert!(!event_log_events(&h).iter().any(
        |event| matches!(event, Event::ShellCommandFinished(finished) if finished.command_id.as_str() == "one")
    ));

    h.tool_routing.registry.register(
        &crate::test_connection_id("extra-shell"),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("extra_shell"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:exec:generic")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        command(agent_id.clone(), &crate::test_connection_id("many")),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(
                |event| matches!(event, Event::ShellCommandFinished(finished)
            if finished.command_id.as_str() == "many"
                && finished.output.contains("multiple"))
            )
            .count(),
        1
    );

    let providers = ui_shell_provider_ids(&h.tool_routing.registry);
    for provider in providers {
        h.tool_routing.registry.unregister_connection(&provider);
    }
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        command(agent_id, &crate::test_connection_id("zero")),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(
                |event| matches!(event, Event::ShellCommandFinished(finished)
            if finished.command_id.as_str() == "zero"
                && finished.output.contains("no shell"))
            )
            .count(),
        1
    );

    h.tool_routing.registry.register(
        &crate::test_connection_id("lost-shell"),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("lost_shell"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:exec:generic")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let target = ensure_test_user_agent(&mut h);
    let target = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&target]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        command(target, &crate::test_connection_id("delivery-lost")),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(
                |event| matches!(event, Event::ShellCommandFinished(finished)
                if finished.command_id.as_str() == "delivery-lost"
                    && finished.output.contains("unavailable"))
            )
            .count(),
        1
    );
}

/// The exact-one user-shell route delivers one concrete-target command only to
/// the selected provider connection.
#[test]
fn ui_shell_route_is_point_to_point_with_resolved_target() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    for provider in ui_shell_provider_ids(&h.tool_routing.registry) {
        h.tool_routing.registry.unregister_connection(&provider);
    }
    let sink = connect_test_tool(&mut h, "shell-provider");
    let decoy = connect_test_tool(&mut h, "shell-decoy");
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("shell-provider"),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::UI_SHELL_COMMAND,
            )],
        )
        .expect("subscribe shell provider");
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("shell-decoy"),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::UI_SHELL_COMMAND,
            )],
        )
        .expect("subscribe shell decoy");
    let ui_a = connect_test_client(&mut h, "shell-ui-a", tau_proto::ClientKind::Ui);
    let ui_b = connect_test_client(&mut h, "shell-ui-b", tau_proto::ClientKind::Ui);
    for ui in ["shell-ui-a", "shell-ui-b"] {
        h.runtime_io
            .bus
            .set_subscriptions(
                &crate::test_connection_id(ui),
                Vec::new(),
                vec![tau_proto::EventSelector::Exact(
                    tau_proto::EventName::UI_SHELL_COMMAND,
                )],
            )
            .expect("subscribe ui projection");
    }
    h.tool_routing.registry.register(
        &crate::test_connection_id("shell-provider"),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("shell"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:exec:generic")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        tau_proto::UiShellCommand {
            session_id: test_session_id("s1"),
            command_id: tau_proto::ShellCommandId::parse("routed-shell")
                .expect("test identifier must satisfy its grammar"),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: None,
        },
    );
    let events = sink.lock().expect("sink");
    let commands = events
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame))
        .filter_map(|event| match event {
            Event::UiShellCommand(command) => Some(command),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].target_agent_id.as_ref(), Some(&agent_id));
    assert!(
        decoy
            .lock()
            .expect("decoy sink")
            .iter()
            .all(|routed| !matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(_))
            ))
    );
    for ui in [&ui_a, &ui_b] {
        assert_eq!(
            ui.lock()
                .expect("ui sink")
                .iter()
                .filter(|routed| matches!(
                    peel_inner_event(&routed.frame),
                    Some(Event::UiShellCommand(command))
                        if command.command_id.as_str() == "routed-shell"
                ))
                .count(),
            1
        );
    }
}

/// User-shell events are accepted only from the selected provider with the
/// harness-owned immutable request identity, and terminal events are one-shot.
#[test]
fn ui_shell_completion_validates_owner_identity_and_exactly_once_terminal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let owner = configure_test_ui_shell_provider(&mut h, "shell-owner");
    connect_test_tool(&mut h, "shell-non-owner");
    let ui = connect_test_client(&mut h, "shell-owner-ui", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("shell-owner-ui"),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::UI_SHELL_COMMAND,
            )],
        )
        .expect("subscribe shell ui");
    let command = routed_ui_shell_command(&mut h, "owned-shell", true);
    h.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());
    assert_eq!(h.ui_runtime.pending_ui_shell_commands.len(), 1);
    let first_route_id = h
        .ui_runtime
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("first provider route")
        .clone();
    let target_agent_id = command.target_agent_id.clone();

    let progress = tau_proto::ShellCommandProgress {
        command_id: first_route_id.as_protocol_id().clone(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "forged".to_owned(),
        target_agent_id: target_agent_id.clone(),
    };
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-non-owner"),
        Event::ShellCommandProgressReported(progress.clone()),
    );
    let mut altered_progress = progress;
    altered_progress.target_agent_id = Some(crate::parse_agent_id("other_agent"));
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandProgressReported(altered_progress),
    );
    assert!(!event_log_events(&h).iter().any(
        |event| matches!(event, Event::ShellCommandProgress(progress)
            if progress.command_id == command.command_id)
    ));
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandProgressReported(tau_proto::ShellCommandProgress {
            command_id: first_route_id.as_protocol_id().clone(),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "mapped".to_owned(),
            target_agent_id: target_agent_id.clone(),
        }),
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ShellCommandProgress(progress)
            if progress.command_id == command.command_id
                && progress.chunk == "mapped"
    )));

    let finished = tau_proto::ShellCommandFinished {
        command_id: first_route_id.as_protocol_id().clone(),
        session_id: command.session_id.clone(),
        command: command.command.clone(),
        include_in_context: command.include_in_context,
        target_agent_id,
        output: "trusted".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-non-owner"),
        Event::ShellCommandFinishedReported(finished.clone()),
    );
    let mut altered_finished = finished.clone();
    altered_finished.include_in_context = false;
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandFinishedReported(altered_finished),
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::ShellCommandFinished(done)
            if done.command_id == command.command_id))
    );

    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandFinishedReported(finished.clone()),
    );
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandFinishedReported(finished.clone()),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::ShellCommandFinished(done)
                if done.command_id == command.command_id))
            .count(),
        1
    );

    h.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());
    let second_route_id = h
        .ui_runtime
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("second provider route")
        .clone();
    assert_ne!(first_route_id, second_route_id);
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandFinishedReported(finished.clone()),
    );
    assert_eq!(
        h.ui_runtime.pending_ui_shell_commands.len(),
        1,
        "late first-route terminal must not consume the reused UI id's route"
    );
    let mut second_finished = finished;
    second_finished.command_id = second_route_id.as_protocol_id().clone();
    second_finished.output = "second".to_owned();
    h.canonicalize_committed_shell_command_report(
        &crate::test_connection_id("shell-owner"),
        Event::ShellCommandFinishedReported(second_finished),
    );
    assert!(h.ui_runtime.pending_ui_shell_commands.is_empty());
    let provider_ids = owner
        .lock()
        .expect("owner sink")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::UiShellCommand(routed)) => Some(routed.command_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        provider_ids.len(),
        2,
        "in-flight duplicates and invalid ids must not reach the provider"
    );
    assert!(provider_ids.iter().all(|id| id != &command.command_id));
    assert_ne!(provider_ids[0], provider_ids[1]);
    assert_eq!(
        ui.lock()
            .expect("ui sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(routed))
                    if routed.command_id == command.command_id
            ))
            .count(),
        2,
        "completed id reuse is a new UI lifecycle; in-flight/invalid ids are not projected"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::ShellCommandFinished(done)
                if done.command_id == command.command_id))
            .count(),
        2,
        "each internal route maps exactly one terminal back to the reused UI id"
    );
}

/// UI shell correlation accepts the validated 64-byte identifier boundary
/// without truncating its provider or UI projection.
#[test]
fn ui_shell_command_id_bounds_apply_before_projection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let provider = configure_test_ui_shell_provider(&mut h, "bounded-shell-owner");
    let ui = connect_test_client(&mut h, "bounded-shell-ui", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("bounded-shell-ui"),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::UI_SHELL_COMMAND,
            )],
        )
        .expect("subscribe ui");
    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = Some(crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    ));
    let session_id = h.session_runtime.current_session_id.clone();
    let command = |id: String| tau_proto::UiShellCommand {
        session_id: session_id.clone(),
        command_id: test_shell_command_id(id),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: target_agent_id.clone(),
    };
    let accepted_id = "a".repeat(64);
    h.handle_ui_shell_command(
        &crate::test_connection_id("ui"),
        command(accepted_id.clone()),
    );

    let provider_commands = provider
        .lock()
        .expect("provider sink")
        .iter()
        .filter(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(_))
            )
        })
        .count();
    assert_eq!(provider_commands, 1);
    let projected = ui
        .lock()
        .expect("ui sink")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::UiShellCommand(command)) => Some(command.command_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        projected,
        vec![tau_proto::ShellCommandId::parse(accepted_id).expect("test identifier must be valid")]
    );
    assert_eq!(h.ui_runtime.pending_ui_shell_commands.len(), 1);
    h.handle_disconnect(&crate::test_connection_id("bounded-shell-owner"));
    assert!(h.ui_runtime.pending_ui_shell_commands.is_empty());
    assert!(h.ui_runtime.active_ui_shell_command_ids.is_empty());
}

/// Disconnect and session shutdown clear pending user-shell ownership and emit
/// one harness-owned terminal failure rather than leaving the UI pending.
#[test]
fn ui_shell_pending_commands_fail_on_provider_disconnect_and_session_shutdown() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    configure_test_ui_shell_provider(&mut h, "shell-owner");
    let disconnected = routed_ui_shell_command(&mut h, "disconnect-shell", false);
    h.handle_disconnect(&crate::test_connection_id("shell-owner"));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::ShellCommandFinished(done)
            if done.command_id == disconnected.command_id
                && done.output.contains("disconnected")))
            .count(),
        1
    );
    assert!(h.ui_runtime.pending_ui_shell_commands.is_empty());

    configure_test_ui_shell_provider(&mut h, "replacement-shell");
    let shutdown = routed_ui_shell_command(&mut h, "shutdown-shell", false);
    h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("switch session");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::ShellCommandFinished(done)
            if done.command_id == shutdown.command_id
                && done.output.contains("session shut down")))
            .count(),
        1
    );
    assert!(h.ui_runtime.pending_ui_shell_commands.is_empty());
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
            inference_activation: false,
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
            resume_inference: false,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "internal prompt should not be listed".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(provider_text_response(
            &test_agent_prompt_id("sp-tree-anchor-parent"),
            agent_id.clone(),
            "assistant answer",
        )),
    );
    let assistant_node = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("assistant node");
    append_user_message_via_event(&mut h, "s1", "second prompt");

    h.handle_ui_navigate_tree(
        &crate::test_connection_id("ui"),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(2),
        },
    )
    .expect("navigate before second prompt");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(assistant_node)
    );

    append_user_message_via_event(&mut h, "s1", "replacement second prompt");
    let branched = default_agent_tree(&h)
        .nodes()
        .last()
        .expect("branched prompt");
    assert_eq!(branched.parent_id, Some(assistant_node));
}

/// Admission rejects malformed request and prompt correlations before creating
/// an agent, while a promptless request succeeds without a prompt correlation.
#[test]
fn ui_create_agent_validates_correlations_and_accepts_promptless_creation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let requester = connect_test_client(&mut h, "correlation-ui", tau_proto::ClientKind::Ui);
    let session_id = h.session_runtime.current_session_id.clone();
    for (request_id, ctx_id) in [
        ("", Some("prompt-correlation")),
        ("missing-prompt-correlation", None),
    ] {
        h.handle_ui_create_agent_from(
            &crate::test_connection_id("correlation-ui"),
            tau_proto::UiCreateAgent {
                request_id: request_id.to_owned(),
                session_id: session_id.clone(),
                role: "engineer".to_owned(),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: Some("hello".to_owned()),
                literal: false,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: ctx_id.map(str::to_owned),
                parent_agent: None,
                ephemeral: false,
            },
        )
        .expect("reject malformed correlation");
    }
    assert!(h.agent_runtime.agent_registry.session_loaded.is_empty());

    for (request_id, request_session, metadata, parent_agent) in [
        (
            "stale-session",
            test_session_id("stale-session"),
            Vec::new(),
            None,
        ),
        (
            "invalid-metadata",
            session_id.clone(),
            vec![tau_proto::AgentInitialMetadata {
                key: tau_proto::AgentMetadataKey::new("oversized"),
                value: tau_proto::CborValue::Bytes(vec![
                    0;
                    tau_proto::MAX_AGENT_METADATA_VALUE_BYTES
                        + 1
                ]),
                inheritable: false,
            }],
            None,
        ),
        (
            "missing-parent",
            session_id.clone(),
            Vec::new(),
            Some(tau_proto::AgentId::parse("missing-parent").expect("agent id")),
        ),
    ] {
        h.handle_ui_create_agent_from(
            &crate::test_connection_id("correlation-ui"),
            tau_proto::UiCreateAgent {
                request_id: request_id.to_owned(),
                session_id: request_session,
                role: "engineer".to_owned(),
                model_override: None,
                metadata,
                initial_prompt: None,
                literal: false,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
                parent_agent,
                ephemeral: false,
            },
        )
        .expect("reject pre-creation request");
    }
    assert!(h.agent_runtime.agent_registry.session_loaded.is_empty());

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("correlation-ui"),
        tau_proto::UiCreateAgent {
            request_id: "promptless-create".to_owned(),
            session_id,
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create promptless agent");
    assert_eq!(h.agent_runtime.agent_registry.session_loaded.len(), 1);
    let outcomes = requester
        .lock()
        .expect("requester frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiCreateAgentResult(result)) => Some(result.outcome.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 6);
    assert!(matches!(
        outcomes[0],
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::InvalidRequestId,
            ..
        }
    ));
    assert!(matches!(
        outcomes[1],
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::InvalidRequestId,
            ..
        }
    ));
    assert!(matches!(
        outcomes[2],
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::StaleSession,
            ..
        }
    ));
    assert!(matches!(
        outcomes[3],
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::InvalidMetadata,
            ..
        }
    ));
    assert!(matches!(
        outcomes[4],
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::ParentNotLoaded,
            ..
        }
    ));
    assert!(matches!(
        outcomes[5],
        tau_proto::UiCreateAgentOutcome::Created {
            initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Absent,
            ..
        }
    ));
}

/// Accepted visible prompts from an authenticated socket UI must perform one
/// unconditional target-only Active write and publish its complete pre-dispatch
/// snapshot across representative prior mode/runtime combinations, including a
/// same-value write.
#[test]
fn accepted_ui_prompt_resumes_exact_target_before_queue_or_dispatch() {
    for (case, prior_mode, turn_state, expected_runtime) in [
        (
            "suspended-idle",
            tau_proto::AgentNavigationMode::Suspended,
            AgentTurnState::Idle,
            tau_proto::AgentRuntimeState::Idle,
        ),
        (
            "auto-idle",
            tau_proto::AgentNavigationMode::ActiveAuto,
            AgentTurnState::Idle,
            tau_proto::AgentRuntimeState::Idle,
        ),
        (
            "auto-running",
            tau_proto::AgentNavigationMode::ActiveAuto,
            AgentTurnState::AgentThinking {
                agent_prompt_id: test_agent_prompt_id("current-auto"),
            },
            tau_proto::AgentRuntimeState::Running,
        ),
        (
            "active-running",
            tau_proto::AgentNavigationMode::Active,
            AgentTurnState::AgentThinking {
                agent_prompt_id: test_agent_prompt_id("current-active"),
            },
            tau_proto::AgentRuntimeState::Running,
        ),
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join(case)).expect("harness");
        let target_cid = ensure_test_user_agent(&mut h);
        let other_cid = h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        );
        let target_id = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&target_cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("target id");
        let other_id = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&other_cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("other id");
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .insert(target_id.clone(), prior_mode);
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .insert(other_id.clone(), tau_proto::AgentNavigationMode::Suspended);
        let target = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&target_cid)
            .expect("target conversation");
        target.turn.turn_state = turn_state;
        target.turn.published_runtime_state = expected_runtime;
        let requester = connect_test_client_with_origin(
            &mut h,
            "prompt-ui",
            tau_proto::ClientKind::Ui,
            ConnectionOrigin::Socket,
        );
        let observer = connect_test_client_with_origin(
            &mut h,
            "prompt-observer",
            tau_proto::ClientKind::Ui,
            ConnectionOrigin::Socket,
        );
        h.runtime_io
            .bus
            .set_subscriptions(
                &crate::test_connection_id("prompt-observer"),
                Vec::new(),
                vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_STATS_UPDATED,
                )],
            )
            .expect("observer subscription");
        requester.lock().expect("requester frames").clear();
        observer.lock().expect("observer frames").clear();

        h.handle_client_event_inner(
            &crate::test_connection_id("prompt-ui"),
            Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: false,
                session_id: h.session_runtime.current_session_id.clone(),
                text: format!("target this agent, not @{other_id}"),
                agent_id: target_id.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some(case.to_owned()),
            }),
        )
        .expect("authenticated UI prompt");

        assert_eq!(
            h.agent_runtime
                .agent_registry
                .navigation_modes
                .get(&target_id),
            Some(&tau_proto::AgentNavigationMode::Active),
            "{case}"
        );
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .navigation_modes
                .get(&other_id),
            Some(&tau_proto::AgentNavigationMode::Suspended),
            "{case}: a textual mention is not a route"
        );
        let target_stats = observer
            .lock()
            .expect("observer frames")
            .iter()
            .filter_map(|frame| match peel_inner_event(&frame.frame) {
                Some(Event::AgentStatsUpdated(stats)) if stats.agent_id == target_id => {
                    Some(stats.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !target_stats.is_empty(),
            "{case}: same-value writes must also publish"
        );
        assert_eq!(
            (
                target_stats[0].navigation_mode,
                target_stats[0].runtime_state
            ),
            (tau_proto::AgentNavigationMode::Active, expected_runtime),
            "{case}: the implicit write snapshot must precede dispatch changes"
        );
        assert!(
            requester
                .lock()
                .expect("requester frames")
                .iter()
                .all(|frame| !matches!(
                    peel_inner_event(&frame.frame),
                    Some(Event::UiSetAgentNavigationModeResult(_))
                )),
            "{case}: implicit writes have no synthetic explicit-write result"
        );

        h.shutdown().expect("shutdown");
    }
}

/// Rejected, synthetic, internal, and unauthenticated UI-shaped prompt frames
/// must not gain the visible-human prompt's shared navigation authority.
#[test]
fn ui_prompt_auto_resume_requires_authenticated_visible_admission() {
    #[derive(Clone, Copy)]
    enum Case {
        StaleSession,
        UnknownTarget,
        TerminatingTarget,
        InvalidSkill,
        InternalClass,
        ExtensionOriginator,
        ExternalPeer,
        PromotedExternalPeer,
        InMemoryUi,
        SocketTool,
        ConfiguredExtension,
        MissingPeer,
        MissingMembership,
        MissingMode,
    }

    for case in [
        Case::StaleSession,
        Case::UnknownTarget,
        Case::TerminatingTarget,
        Case::InvalidSkill,
        Case::InternalClass,
        Case::ExtensionOriginator,
        Case::ExternalPeer,
        Case::PromotedExternalPeer,
        Case::InMemoryUi,
        Case::SocketTool,
        Case::ConfiguredExtension,
        Case::MissingPeer,
        Case::MissingMembership,
        Case::MissingMode,
    ] {
        let case_name = match case {
            Case::StaleSession => "stale-session",
            Case::UnknownTarget => "unknown-target",
            Case::TerminatingTarget => "terminating-target",
            Case::InvalidSkill => "invalid-skill",
            Case::InternalClass => "internal-class",
            Case::ExtensionOriginator => "extension-originator",
            Case::ExternalPeer => "external-peer",
            Case::PromotedExternalPeer => "promoted-external-peer",
            Case::InMemoryUi => "in-memory-ui",
            Case::SocketTool => "socket-tool",
            Case::ConfiguredExtension => "configured-extension",
            Case::MissingPeer => "missing-peer",
            Case::MissingMembership => "missing-membership",
            Case::MissingMode => "missing-mode",
        };
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join(case_name)).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        let target_id = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("target id");
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .insert(target_id.clone(), tau_proto::AgentNavigationMode::Suspended);
        let observer = connect_test_client(&mut h, "prompt-observer", tau_proto::ClientKind::Ui);
        h.runtime_io
            .bus
            .set_subscriptions(
                &crate::test_connection_id("prompt-observer"),
                Vec::new(),
                vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_STATS_UPDATED,
                )],
            )
            .expect("observer subscription");
        observer.lock().expect("observer frames").clear();
        let source_id = match case {
            Case::ExternalPeer => {
                connect_test_client_with_origin(
                    &mut h,
                    "prompt-source",
                    tau_proto::ClientKind::External,
                    ConnectionOrigin::Socket,
                );
                "prompt-source"
            }
            Case::PromotedExternalPeer => {
                connect_test_client_with_origin(
                    &mut h,
                    "prompt-source",
                    tau_proto::ClientKind::Ui,
                    ConnectionOrigin::Socket,
                );
                h.peer_messaging
                    .external_message_peers
                    .insert(crate::test_connection_id("prompt-source"));
                "prompt-source"
            }
            Case::InMemoryUi => {
                connect_test_client_with_origin(
                    &mut h,
                    "prompt-source",
                    tau_proto::ClientKind::Ui,
                    ConnectionOrigin::InMemory,
                );
                "prompt-source"
            }
            Case::SocketTool => {
                connect_test_client_with_origin(
                    &mut h,
                    "prompt-source",
                    tau_proto::ClientKind::Tool,
                    ConnectionOrigin::Socket,
                );
                "prompt-source"
            }
            Case::ConfiguredExtension => {
                connect_ready_configured_extension(
                    &mut h,
                    "prompt-source",
                    "prompt-extension",
                    tau_proto::ClientKind::Tool,
                );
                "prompt-source"
            }
            Case::MissingPeer => "prompt-source",
            _ => {
                connect_test_client_with_origin(
                    &mut h,
                    "prompt-source",
                    tau_proto::ClientKind::Ui,
                    ConnectionOrigin::Socket,
                );
                "prompt-source"
            }
        };
        if matches!(case, Case::TerminatingTarget) {
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("target")
                .dispatch
                .terminating = true;
        }
        if matches!(case, Case::MissingMembership) {
            h.agent_runtime
                .agent_registry
                .session_loaded
                .remove(&target_id);
        }
        if matches!(case, Case::MissingMode) {
            h.agent_runtime
                .agent_registry
                .navigation_modes
                .remove(&target_id);
        }
        let session_id = if matches!(case, Case::StaleSession) {
            test_session_id("stale-session")
        } else {
            h.session_runtime.current_session_id.clone()
        };
        let submitted_agent_id = if matches!(case, Case::UnknownTarget) {
            tau_proto::AgentId::parse("unknown-target").expect("agent id")
        } else {
            target_id.clone()
        };
        let text = if matches!(case, Case::InvalidSkill) {
            ":skill definitely-not-installed".to_owned()
        } else {
            case_name.to_owned()
        };
        let message_class = if matches!(case, Case::InternalClass) {
            tau_proto::PromptMessageClass::Internal
        } else {
            tau_proto::PromptMessageClass::User
        };
        let originator = if matches!(case, Case::ExtensionOriginator) {
            tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("synthetic"),
                query_id: "side-query".to_owned(),
            }
        } else {
            tau_proto::PromptOriginator::User
        };

        let prompt_event = Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id,
            text,
            agent_id: submitted_agent_id,
            message_class,
            originator,
            ctx_id: Some(case_name.to_owned()),
        });
        if matches!(case, Case::ConfiguredExtension) {
            h.handle_extension_message(
                &crate::test_connection_id(source_id),
                TestMessage::Emit(tau_proto::Emit {
                    event: Box::new(prompt_event),
                    persist: false,
                }),
            )
            .expect("configured-extension prompt-shaped Emit");
        } else {
            h.handle_client_event_inner(&crate::test_connection_id(source_id), prompt_event)
                .expect("UI-shaped prompt frame");
        }

        if matches!(case, Case::MissingMode) {
            assert!(
                !h.agent_runtime
                    .agent_registry
                    .navigation_modes
                    .contains_key(&target_id),
                "{case_name}"
            );
        } else {
            assert_eq!(
                h.agent_runtime
                    .agent_registry
                    .navigation_modes
                    .get(&target_id),
                Some(&tau_proto::AgentNavigationMode::Suspended),
                "{case_name}"
            );
        }
        assert!(
            observer
                .lock()
                .expect("observer frames")
                .iter()
                .all(|frame| !matches!(
                    peel_inner_event(&frame.frame),
                    Some(Event::AgentStatsUpdated(stats))
                        if stats.agent_id == target_id
                            && stats.navigation_mode == tau_proto::AgentNavigationMode::Active
                )),
            "{case_name}: no Active snapshot may be published"
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent_events(target_id.as_str())
                .expect("agent journal")
                .into_iter()
                .filter(|record| matches!(record.event, Event::AgentUserInteractionRecorded(_)))
                .count(),
            0,
            "{case_name}: no visible-interaction admission marker"
        );
        let accepted_nonvisible = matches!(case, Case::InternalClass | Case::ExtensionOriginator);
        if accepted_nonvisible {
            assert!(
                event_log_events(&h).iter().any(|event| matches!(
                    event,
                    Event::AgentPromptSubmitted(prompt)
                        if prompt.ctx_id.as_deref() == Some(case_name)
                )),
                "{case_name}: accepted non-visible prompts still execute"
            );
        } else {
            assert!(
                event_log_events(&h).iter().all(|event| match event {
                    Event::AgentPromptSubmitted(prompt) => {
                        prompt.ctx_id.as_deref() != Some(case_name)
                    }
                    Event::AgentPromptSteered(prompt) => {
                        prompt.ctx_id.as_deref() != Some(case_name)
                    }
                    _ => true,
                }),
                "{case_name}: unauthorized or invalid input must not execute"
            );
            assert!(
                h.agent_runtime
                    .agent_registry
                    .agents
                    .values()
                    .all(|agent| agent
                        .dispatch
                        .pending_prompts
                        .iter()
                        .all(|prompt| prompt.ctx_id.as_deref() != Some(case_name))),
                "{case_name}: unauthorized or invalid input must not queue"
            );
        }

        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("target")
            .dispatch
            .terminating = false;
        h.agent_runtime
            .agent_registry
            .session_loaded
            .insert(target_id.clone());
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .entry(target_id)
            .or_insert(tau_proto::AgentNavigationMode::Suspended);
        h.shutdown().expect("shutdown");
    }
}

/// A failed durable accepted-interaction append must abort before the implicit
/// navigation write, stats publication, or prompt admission.
#[test]
fn ui_prompt_interaction_append_failure_does_not_resume_or_admit() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = echo_harness(&state_dir).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let target_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|agent| agent.identity.agent_id.as_deref())
        .map(crate::parse_agent_id)
        .expect("target id");
    h.agent_runtime
        .agent_registry
        .navigation_modes
        .insert(target_id.clone(), tau_proto::AgentNavigationMode::Suspended);
    connect_test_client_with_origin(
        &mut h,
        "append-failure-ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let observer =
        connect_test_client(&mut h, "append-failure-observer", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("append-failure-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    observer.lock().expect("observer frames").clear();

    let failure_store = state_dir.join("interaction-failure-agent-store");
    let mut agent_store = tau_core::AgentStore::open(&failure_store).expect("failure agent store");
    agent_store
        .append_agent_event(
            target_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                parent_agent: None,
                agent_id: target_id.clone(),
                role: h.config.selected_role.clone(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed failure agent");
    let event_path = failure_store.join(target_id.as_str()).join("events.cbor");
    let backup_path = event_path.with_extension("cbor.interaction-backup");
    std::fs::rename(&event_path, &backup_path).expect("park agent journal");
    std::fs::create_dir(&event_path).expect("block agent append with directory");
    h.session_runtime.agent_store = agent_store;

    let result = h.handle_client_event_inner(
        &crate::test_connection_id("append-failure-ui"),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "must not be admitted".to_owned(),
            agent_id: target_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("interaction-append-failure".to_owned()),
        }),
    );
    std::fs::remove_dir(&event_path).expect("remove append blocker");
    std::fs::rename(&backup_path, &event_path).expect("restore agent journal");

    assert!(matches!(result, Err(HarnessError::AgentStore(_))));
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&target_id),
        Some(&tau_proto::AgentNavigationMode::Suspended)
    );
    assert!(
        observer
            .lock()
            .expect("observer frames")
            .iter()
            .all(|frame| !matches!(
                peel_inner_event(&frame.frame),
                Some(Event::AgentStatsUpdated(stats))
                    if stats.agent_id == target_id
                        && stats.navigation_mode == tau_proto::AgentNavigationMode::Active
            ))
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(target_id.as_str())
            .expect("agent journal")
            .into_iter()
            .filter(|record| matches!(record.event, Event::AgentUserInteractionRecorded(_)))
            .count(),
        0
    );
    assert!(event_log_events(&h).iter().all(|event| match event {
        Event::AgentPromptSubmitted(prompt) => {
            prompt.ctx_id.as_deref() != Some("interaction-append-failure")
        }
        Event::AgentPromptSteered(prompt) => {
            prompt.ctx_id.as_deref() != Some("interaction-append-failure")
        }
        _ => true,
    }));
    assert!(h.agent_runtime.agent_registry.agents.values().all(|agent| {
        agent
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.ctx_id.as_deref() != Some("interaction-append-failure"))
    }));

    h.shutdown().expect("shutdown");
}

/// UI activation appends emit one fixed, content-free trace whether they
/// dispatch immediately or wait in the queue, while internal queue traffic
/// remains outside the prompt-acceptance diagnostic.
#[test]
fn ui_activation_append_traces_cover_immediate_and_queued_prompts_only() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(temp.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");
    let expected_trace_agent_id = cid.to_string();
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(TraceCaptureLayer {
        capture: capture.clone(),
    });

    tracing::subscriber::with_default(subscriber, || {
        let activation_count = || {
            capture
                .events
                .lock()
                .expect("trace capture lock")
                .iter()
                .filter(|trace| trace.fields.contains_key("activation_append_us"))
                .count()
        };
        submit_authenticated_ui_prompt(
            &mut harness,
            crate::parse_agent_id(&agent_id),
            "IMMEDIATE-UI-CANARY",
            tau_proto::PromptMessageClass::User,
        )
        .expect("submit immediate UI prompt");
        assert_eq!(activation_count(), 1, "immediate UI append traces once");
        assert_eq!(
            capture.events.lock().expect("trace capture lock").len(),
            2,
            "immediate UI dispatch also traces its durable session touch once"
        );

        submit_authenticated_ui_prompt(
            &mut harness,
            crate::parse_agent_id(&agent_id),
            "QUEUED-UI-CANARY",
            tau_proto::PromptMessageClass::User,
        )
        .expect("queue UI prompt");
        assert_eq!(
            activation_count(),
            2,
            "the startup-or-global-turn queued UI branch traces once"
        );
        harness.session_runtime.turn_state = TurnState::Idle;
        submit_authenticated_ui_prompt(
            &mut harness,
            crate::parse_agent_id(&agent_id),
            "BLOCKED-UI-CANARY",
            tau_proto::PromptMessageClass::User,
        )
        .expect("queue blocked UI prompt");
        assert_eq!(
            activation_count(),
            3,
            "the per-agent dispatch-blocked queued UI branch traces once"
        );
        submit_authenticated_ui_prompt(
            &mut harness,
            crate::parse_agent_id(&agent_id),
            "INTERNAL-QUEUE-CANARY",
            tau_proto::PromptMessageClass::Internal,
        )
        .expect("queue internal UI prompt");
        assert_eq!(
            activation_count(),
            3,
            "queued internal activation remains outside the UI diagnostic"
        );
        assert_eq!(
            harness.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_prompts
                .iter()
                .map(|prompt| prompt.text.as_str())
                .collect::<Vec<_>>(),
            [
                "QUEUED-UI-CANARY",
                "BLOCKED-UI-CANARY",
                "INTERNAL-QUEUE-CANARY"
            ],
            "instrumentation must not change queue order"
        );

        harness.session_runtime.turn_state = TurnState::Idle;
        let agent = harness
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.turn.turn_state = AgentTurnState::Idle;
        agent.dispatch.in_flight_prompt = None;
        agent.dispatch.activation_dispatch = ActivationDispatchState::None;
        harness.try_advance_queue();
        assert_eq!(
            capture
                .events
                .lock()
                .expect("trace capture lock")
                .iter()
                .filter(|trace| trace.fields.contains_key("session_meta_touch_us"))
                .count(),
            2,
            "the drained queued UI prompt touches session metadata once"
        );
        assert_eq!(
            activation_count(),
            3,
            "queue draining must not append a second UI activation observation"
        );

        harness
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .identity
            .agent_id = None;
        harness.append_prompt_activation_queued(
            &cid,
            tau_proto::ObservationId::random(),
            tau_proto::ActivationKind::VisibleUser,
            &PendingPrompt::human_ui("FAILED-UI-CANARY".to_owned()),
        );
        assert_eq!(activation_count(), 4, "failed append traces once");
    });

    let captured_traces = capture.events.lock().expect("trace capture lock");
    assert_eq!(
        captured_traces.len(),
        6,
        "the immediate and drained queued UI dispatches plus all activation results are the entire trace"
    );
    let activation_traces = captured_traces
        .iter()
        .filter(|trace| trace.fields.contains_key("activation_append_us"))
        .collect::<Vec<_>>();
    assert_eq!(
        activation_traces.len(),
        4,
        "immediate UI, both queued UI branches, and forced append failure trace exactly once"
    );
    assert!(
        activation_traces.iter().all(|trace| {
            trace.target == "tau_harness::prompt_acceptance"
                && trace.fields.keys().map(String::as_str).eq([
                    "activation_append_us",
                    "agent_id",
                    "event_class",
                    "message",
                    "result_class",
                    "stage",
                ])
                && trace
                    .fields
                    .get("stage")
                    .is_some_and(|value| value == "activation_append")
                && trace
                    .fields
                    .get("event_class")
                    .is_some_and(|value| value == "agent.activation_queued")
                && trace
                    .fields
                    .get("message")
                    .is_some_and(|value| value == "content-free prompt acceptance precursor")
                && trace
                    .fields
                    .get("agent_id")
                    .is_some_and(|value| value == &expected_trace_agent_id)
                && trace
                    .fields
                    .get("activation_append_us")
                    .is_some_and(|value| value.parse::<u128>().is_ok())
        }),
        "every record keeps the fixed prompt-acceptance classes: {activation_traces:?}"
    );
    assert_eq!(
        activation_traces
            .iter()
            .filter(|trace| {
                trace
                    .fields
                    .get("result_class")
                    .is_some_and(|value| value == "success")
            })
            .count(),
        3,
        "immediate and both queued UI paths retain success classification"
    );
    assert_eq!(
        activation_traces
            .iter()
            .filter(|trace| {
                trace
                    .fields
                    .get("result_class")
                    .is_some_and(|value| value == "failure")
            })
            .count(),
        1,
        "failed append retains failure classification"
    );
    let session_meta_traces = captured_traces
        .iter()
        .filter(|trace| trace.fields.contains_key("session_meta_touch_us"))
        .collect::<Vec<_>>();
    assert_eq!(
        session_meta_traces.len(),
        2,
        "the immediate and drained queued durable UI dispatches touch session metadata once"
    );
    assert!(
        session_meta_traces.iter().all(|trace| {
            trace.target == "tau_harness::prompt_acceptance"
                && trace.fields.keys().map(String::as_str).eq([
                    "agent_id",
                    "message",
                    "result_class",
                    "session_meta_touch_us",
                    "stage",
                ])
                && trace
                    .fields
                    .get("stage")
                    .is_some_and(|value| value == "session_meta_touch")
                && trace
                    .fields
                    .get("result_class")
                    .is_some_and(|value| value == "success")
                && trace
                    .fields
                    .get("message")
                    .is_some_and(|value| value == "content-free prompt acceptance precursor")
                && trace
                    .fields
                    .get("agent_id")
                    .is_some_and(|value| value == &expected_trace_agent_id)
                && trace
                    .fields
                    .get("session_meta_touch_us")
                    .is_some_and(|value| value.parse::<u128>().is_ok())
        }),
        "session metadata traces retain their distinct fixed fields: {session_meta_traces:?}"
    );
    assert!(
        captured_traces.iter().all(|trace| {
            !trace.fields.values().any(|value| {
                value.contains("IMMEDIATE-UI-CANARY")
                    || value.contains("QUEUED-UI-CANARY")
                    || value.contains("BLOCKED-UI-CANARY")
                    || value.contains("INTERNAL-QUEUE-CANARY")
                    || value.contains("FAILED-UI-CANARY")
            })
        }),
        "trace output must not retain prompt content"
    );
    harness.shutdown().expect("shutdown");
}

/// Only authenticated UI submissions may enter prompt-acceptance traces, even
/// when other activating or passive prompts dispatch immediately.
#[test]
fn prompt_acceptance_traces_exclude_non_ui_prompt_and_metadata_traffic() {
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(TraceCaptureLayer {
        capture: capture.clone(),
    });

    tracing::subscriber::with_default(subscriber, || {
        let submit = |prompt: PendingPrompt| {
            let temp = TempDir::new().expect("tempdir");
            let mut harness = echo_harness(temp.path().join("state")).expect("harness");
            let cid = ensure_test_user_agent(&mut harness);
            let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
                .identity
                .agent_id
                .clone()
                .expect("durable agent id");
            let submission = harness
                .submit_prompt_to_agent(
                    harness.session_runtime.current_session_id.clone(),
                    agent_id.as_str(),
                    prompt,
                )
                .expect("submit prompt");
            harness.shutdown().expect("shutdown");
            submission
        };
        let trace_count = || capture.events.lock().expect("trace capture lock").len();

        let temp = TempDir::new().expect("tempdir");
        let mut harness = echo_harness(temp.path().join("state")).expect("harness");
        let cid = ensure_test_user_agent(&mut harness);
        let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .map(crate::parse_agent_id)
            .expect("durable agent id");
        submit_authenticated_ui_prompt(
            &mut harness,
            agent_id,
            "IMMEDIATE-INTERNAL-CANARY",
            tau_proto::PromptMessageClass::Internal,
        )
        .expect("submit authenticated internal UI prompt");
        harness.shutdown().expect("shutdown");
        assert_eq!(
            trace_count(),
            0,
            "authenticated UI internal prompt remains outside UI diagnostics"
        );

        assert_eq!(
            submit(PendingPrompt::user(
                "IMMEDIATE-HARNESS-USER-CANARY".to_owned()
            )),
            PromptSubmission::Dispatched
        );
        assert_eq!(
            trace_count(),
            0,
            "visible harness-owned prompt remains outside UI diagnostics"
        );

        let mut extension_prompt = PendingPrompt::user("IMMEDIATE-EXTENSION-CANARY".to_owned());
        extension_prompt.submission_source = tau_proto::PromptSubmissionSource::Extension {
            name: tau_proto::ExtensionName::parse("trace-test").expect("extension name"),
        };
        assert_eq!(submit(extension_prompt), PromptSubmission::Dispatched);
        assert_eq!(
            trace_count(),
            0,
            "immediate extension activation remains outside UI diagnostics"
        );

        let temp = TempDir::new().expect("tempdir");
        let mut harness = echo_harness(temp.path().join("state")).expect("harness");
        harness
            .handle_ui_create_agent_from(
                &crate::test_connection_id("trace-ui-create"),
                tau_proto::UiCreateAgent {
                    request_id: "trace-agent-create".to_owned(),
                    literal: true,
                    session_id: harness.session_runtime.current_session_id.clone(),
                    role: harness.config.selected_role.clone(),
                    model_override: None,
                    metadata: Vec::new(),
                    initial_prompt: Some("AGENT-CREATE-CANARY".to_owned()),
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    ctx_id: Some("trace-agent-create-context".to_owned()),
                    parent_agent: None,
                    ephemeral: false,
                },
            )
            .expect("create agent through the UI handler");
        harness.shutdown().expect("shutdown");
        assert_eq!(
            trace_count(),
            0,
            "agent-create activation remains outside UI diagnostics"
        );

        assert_eq!(
            submit(PendingPrompt::passive_background_completion(
                "PASSIVE-CANARY".to_owned()
            )),
            PromptSubmission::Dispatched
        );
        assert_eq!(
            trace_count(),
            0,
            "passive prompt dispatch remains outside UI diagnostics"
        );
    });

    assert!(
        capture
            .events
            .lock()
            .expect("trace capture lock")
            .is_empty(),
        "non-UI canaries must not appear in content-free trace output"
    );
}

/// Existing-agent UI intake keeps the accepted fact and navigation preview raw
/// while every provider receives the typed HumanUi `<user>` projection.
#[test]
fn existing_agent_human_ui_prompt_is_wrapped_only_in_provider_context() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let raw = "  hello <world> & 雪\nnext  ";

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: raw.to_owned(),
            agent_id: agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("ui-existing".to_owned()),
        },
    )
    .expect("submit existing-agent UI prompt");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == raw
                && submitted.submission_source
                    == tau_proto::PromptSubmissionSource::HumanUi
    )));
    let prompt = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id => Some(prompt),
            _ => None,
        })
        .expect("provider prompt");
    assert!(
        prompt
            .context
            .flatten()
            .iter()
            .any(|item| { text_part(item) == Some("<user>  hello <world> & 雪\nnext  </user>") })
    );
    let tree = h.tree_request_result(&test_session_id("s1"), Some(agent_id.as_str()));
    assert!(tree.contains("hello <world> & 雪"));
    assert!(
        !tree.contains("<user>"),
        "tree preview must not expose provider markup"
    );
    h.shutdown().expect("shutdown");
}

/// New-agent initial UI intake follows the same HumanUi provider projection as
/// an existing-agent submission without storing provider markup in the journal.
#[test]
fn new_agent_initial_human_ui_prompt_is_wrapped_only_in_provider_context() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.config.selected_model = Some("test/model".into());
    let raw = "initial <prompt> & text";

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: false,
            parent_agent: None,
            session_id: test_session_id("s1"),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(raw.to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("ui-new".to_owned()),
            ephemeral: false,
        },
    )
    .expect("create agent with initial UI prompt");

    let submitted = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptSubmitted(submitted) if submitted.text == raw => Some(submitted),
            _ => None,
        })
        .expect("canonical initial prompt");
    assert_eq!(
        submitted.submission_source,
        tau_proto::PromptSubmissionSource::HumanUi
    );
    let prompt = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == submitted.agent_id => {
                Some(prompt)
            }
            _ => None,
        })
        .expect("initial provider prompt");
    assert!(
        prompt
            .context
            .flatten()
            .iter()
            .any(|item| { text_part(item) == Some("<user>initial <prompt> & text</user>") })
    );
    assert!(
        prompt
            .context
            .flatten()
            .iter()
            .all(|item| text_part(item) != Some(raw)),
        "provider context must not retain a second raw copy"
    );
    h.shutdown().expect("shutdown");
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
/// Ensures all three UI actions are absolute shared writes while stale-session
/// and extension-originated requests cannot mutate the loaded agent's mode.
#[test]
fn shared_agent_navigation_mode_writes_are_ui_only_and_absolute() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let requester = connect_test_client(&mut h, "navigation-ui", tau_proto::ClientKind::Ui);
    let observer = connect_test_client(&mut h, "navigation-observer", tau_proto::ClientKind::Ui);
    let external = connect_test_client(
        &mut h,
        "navigation-external",
        tau_proto::ClientKind::External,
    );
    let promoted_external =
        connect_test_client(&mut h, "navigation-promoted", tau_proto::ClientKind::Ui);
    let promoted_external_id = h
        .runtime_io
        .bus
        .connections()
        .into_iter()
        .find(|connection| connection.name == "navigation-promoted")
        .expect("promoted external connection")
        .id;
    h.peer_messaging
        .external_message_peers
        .insert(promoted_external_id.clone());
    h.submit_user_prompt(
        h.session_runtime.current_session_id.clone(),
        "hello".to_owned(),
    )
    .expect("create user agent");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .session_loaded
        .iter()
        .next()
        .cloned()
        .expect("loaded agent");
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("navigation-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    requester.lock().expect("requester frames").clear();
    observer.lock().expect("observer frames").clear();
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );

    for (index, action, expected) in [
        (
            1,
            tau_proto::UiAgentNavigationModeAction::SetSuspended,
            tau_proto::AgentNavigationMode::Suspended,
        ),
        (
            2,
            tau_proto::UiAgentNavigationModeAction::SetActiveAuto,
            tau_proto::AgentNavigationMode::ActiveAuto,
        ),
        (
            3,
            tau_proto::UiAgentNavigationModeAction::SetActive,
            tau_proto::AgentNavigationMode::Active,
        ),
    ] {
        h.handle_client_event_inner(
            &crate::test_connection_id("navigation-ui"),
            Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
                request_id: format!("navigation-{index}"),
                session_id: h.session_runtime.current_session_id.clone(),
                agent_id: agent_id.clone(),
                action,
            }),
        )
        .expect("UI navigation request");
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .navigation_modes
                .get(&agent_id),
            Some(&expected)
        );
    }

    h.handle_client_event_inner(
        &crate::test_connection_id("navigation-ui"),
        Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
            request_id: "stale".to_owned(),
            session_id: test_session_id("other-session"),
            agent_id: agent_id.clone(),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        }),
    )
    .expect("stale UI request");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );

    h.handle_client_event_inner(
        &crate::test_connection_id("navigation-ui"),
        Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
            request_id: "unloaded".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: tau_proto::AgentId::parse("unknown-agent").expect("agent id"),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        }),
    )
    .expect("unloaded request");

    h.handle_client_event_inner(
        &crate::test_connection_id("navigation-external"),
        Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
            request_id: "external".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        }),
    )
    .expect("external request consumed");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    assert!(external.lock().expect("external frames").is_empty());
    h.handle_client_event_inner(
        &promoted_external_id,
        Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
            request_id: "promoted-external".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        }),
    )
    .expect("promoted external request consumed");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    assert!(
        promoted_external
            .lock()
            .expect("promoted external frames")
            .is_empty()
    );
    let results = requester
        .lock()
        .expect("requester frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiSetAgentNavigationModeResult(result)) => Some(result.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 5);
    assert_eq!(
        results
            .iter()
            .map(|result| (result.request_id.as_str(), result.outcome))
            .collect::<Vec<_>>(),
        vec![
            (
                "navigation-1",
                tau_proto::UiSetAgentNavigationModeOutcome::Applied,
            ),
            (
                "navigation-2",
                tau_proto::UiSetAgentNavigationModeOutcome::Applied,
            ),
            (
                "navigation-3",
                tau_proto::UiSetAgentNavigationModeOutcome::Applied,
            ),
            (
                "stale",
                tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
                    reason: tau_proto::UiSetAgentNavigationModeRejection::StaleSession,
                },
            ),
            (
                "unloaded",
                tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
                    reason: tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded,
                },
            ),
        ]
    );
    assert!(
        !observer
            .lock()
            .expect("observer frames")
            .iter()
            .any(|frame| {
                matches!(
                    peel_inner_event(&frame.frame),
                    Some(Event::UiSetAgentNavigationModeResult(_))
                )
            })
    );
    let observed_modes = observer
        .lock()
        .expect("observer frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::AgentStatsUpdated(stats)) if stats.agent_id == agent_id => {
                Some(stats.navigation_mode)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed_modes,
        vec![
            tau_proto::AgentNavigationMode::Suspended,
            tau_proto::AgentNavigationMode::ActiveAuto,
            tau_proto::AgentNavigationMode::Active,
        ]
    );
    h.handle_client_event_inner(
        &crate::test_connection_id("navigation-ui"),
        Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
            request_id: "before-reconnect".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        }),
    )
    .expect("set mode before reconnect");
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "resume before reconnect".to_owned(),
            agent_id: agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("navigation-reconnect".to_owned()),
        },
    )
    .expect("accepted prompt before reconnect");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    h.runtime_io
        .bus
        .disconnect(&crate::test_connection_id("navigation-ui"));
    let reconnected =
        connect_test_client(&mut h, "navigation-reconnected", tau_proto::ClientKind::Ui);
    h.complete_subscription(
        &crate::test_connection_id("navigation-reconnected"),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("reconnected catch-up");
    assert!(
        reconnected
            .lock()
            .expect("reconnected frames")
            .iter()
            .any(|frame| matches!(
                peel_inner_event(&frame.frame),
                Some(Event::AgentStatsUpdated(stats))
                    if stats.agent_id == agent_id
                        && stats.navigation_mode == tau_proto::AgentNavigationMode::Active
            ))
    );

    let extension_event = Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
        request_id: "extension".to_owned(),
        session_id: h.session_runtime.current_session_id.clone(),
        agent_id: agent_id.clone(),
        action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
    });
    connect_test_tool(&mut h, "extension-test");
    h.handle_extension_event_inner(
        &crate::test_connection_id("extension-test"),
        extension_event,
    )
    .expect("extension intake");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    h.react_to_committed_event(
        None,
        &Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
        }),
        true,
        None,
    );
    assert!(
        !h.agent_runtime
            .agent_registry
            .navigation_modes
            .contains_key(&agent_id)
    );
    h.agent_runtime
        .agent_registry
        .navigation_modes
        .insert(agent_id.clone(), tau_proto::AgentNavigationMode::Suspended);
    h.switch_session(
        test_session_id("navigation-next"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    assert!(h.agent_runtime.agent_registry.navigation_modes.is_empty());
    h.shutdown().expect("shutdown");
}

/// Ensures create admission returns exactly one correlated point-to-point
/// rejection and never exposes that transient result to another UI.
#[test]
fn ui_create_agent_rejection_is_correlated_and_point_to_point() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let requester = connect_test_client(&mut h, "create-requester", tau_proto::ClientKind::Ui);
    let observer = connect_test_client(&mut h, "create-observer", tau_proto::ClientKind::Ui);

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("create-requester"),
        tau_proto::UiCreateAgent {
            request_id: "create-missing-role".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "missing-role".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("never admitted".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("create-missing-role-prompt".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create rejection");

    let results = requester
        .lock()
        .expect("requester frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiCreateAgentResult(result)) => Some(result.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].request_id, "create-missing-role");
    assert!(matches!(
        results[0].outcome,
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::RoleUnavailable,
            agent_id: None,
            ..
        }
    ));
    assert!(
        observer
            .lock()
            .expect("observer frames")
            .iter()
            .all(|frame| !matches!(
                peel_inner_event(&frame.frame),
                Some(Event::UiCreateAgentResult(_))
            ))
    );
    assert!(h.agent_runtime.agent_registry.session_loaded.is_empty());
}

/// Ensures preprocessing failure follows successful queued admission as a
/// separately correlated prompt terminal.
#[test]
fn ui_create_agent_skill_rejection_reports_partial_creation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let requester = connect_test_client(&mut h, "skill-requester", tau_proto::ClientKind::Ui);

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("skill-requester"),
        tau_proto::UiCreateAgent {
            request_id: "create-missing-skill".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "engineer".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(":skill missing-skill".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("create-missing-skill-prompt".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create with invalid skill");
    h.try_advance_queue();

    let result = requester
        .lock()
        .expect("requester frames")
        .iter()
        .find_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiCreateAgentResult(result)) => Some(result.clone()),
            _ => None,
        })
        .expect("directed create result");
    let tau_proto::UiCreateAgentOutcome::Created {
        agent_id,
        initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
    } = result.outcome
    else {
        panic!("expected queued create admission");
    };
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&agent_id)
    );
    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-missing-skill"
                && failed.agent_id == agent_id
                && failed.ctx_id == "create-missing-skill-prompt"
                && failed.stage == tau_proto::AgentPromptFailureStage::Preprocessing
    )));
    assert!(event_log_events(&h).into_iter().all(|event| !matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.agent_id == agent_id
    )));
    let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("after missing skill".to_owned()))
        .expect("dispatch later no-ctx prompt");
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.ctx_id.as_deref() == Some("create-missing-skill-prompt")
    )));
    assert!(read_nth_prompt_created(&h, 0).ctx_id.is_none());
}
/// A resumed agent accepts UI-owned branch reselection and extends the selected
/// durable per-agent head rather than the process-global cursor.
#[test]
fn ui_navigate_tree_can_reselect_agent_head_after_resume() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let first_user_head;
    let agent_id: tau_proto::AgentId;

    {
        let mut h = echo_harness(&sp).expect("start");

        append_user_message_via_event(&mut h, "s1", "first branch point");
        let cid = ensure_test_user_agent(&mut h);
        first_user_head = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("first user head");
        append_user_message_via_event(&mut h, "s1", "second branch point");
        agent_id = crate::parse_agent_id(
            h.agent_runtime.agent_registry.agents[&cid]
                .identity
                .agent_id
                .clone()
                .expect("default conversation agent id"),
        );

        h.handle_ui_navigate_tree(
            &crate::test_connection_id("ui"),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Node(first_user_head),
            },
        )
        .expect("navigate tree");

        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            Some(first_user_head)
        );
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
            &crate::test_connection_id("ui"),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Node(first_user_head),
            },
        )
        .expect("reselect tree head after resume");

        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            Some(first_user_head)
        );

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
            &crate::test_connection_id("ui"),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Root,
            },
        )
        .expect("navigate to root");
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            None
        );
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
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            None
        );

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
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("factory-ext"),
            Vec::new(),
            vec![EventSelector::Prefix("factory.".to_owned())],
        )
        .expect("extension subscription");
    let _ui = connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );

    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
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

/// A failed retention-hint touch remains one content-free UI diagnostic and
/// does not retract the accepted UI prompt dispatch.
#[test]
fn ui_session_metadata_touch_failure_is_traced_without_blocking_dispatch() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(temp.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    let expected_trace_agent_id = cid.to_string();
    let meta_path = harness
        .session_runtime
        .store
        .sessions_dir()
        .join(harness.session_runtime.current_session_id.as_str())
        .join("meta.json");
    path_std_fs::remove_file(&meta_path).expect("remove session metadata");
    path_std_fs::create_dir(&meta_path).expect("obstruct session metadata touch");
    let capture = TraceCapture::default();
    let subscriber = tracing_subscriber::registry().with(TraceCaptureLayer {
        capture: capture.clone(),
    });

    tracing::subscriber::with_default(subscriber, || {
        submit_authenticated_ui_prompt(
            &mut harness,
            agent_id,
            "SESSION-META-FAILURE-CANARY",
            tau_proto::PromptMessageClass::User,
        )
        .expect("dispatch continues after metadata touch failure");
    });

    let captured_traces = capture.events.lock().expect("trace capture lock");
    assert_eq!(
        captured_traces.len(),
        2,
        "one successful activation append and one failed metadata touch are emitted"
    );
    assert!(captured_traces.iter().any(|trace| {
        trace.fields.contains_key("activation_append_us")
            && trace
                .fields
                .get("result_class")
                .is_some_and(|value| value == "success")
    }));
    let session_meta_traces = captured_traces
        .iter()
        .filter(|trace| trace.fields.contains_key("session_meta_touch_us"))
        .collect::<Vec<_>>();
    assert_eq!(session_meta_traces.len(), 1);
    let session_meta_trace = session_meta_traces[0];
    assert_eq!(session_meta_trace.target, "tau_harness::prompt_acceptance");
    assert_eq!(
        session_meta_trace
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "agent_id",
            "message",
            "result_class",
            "session_meta_touch_us",
            "stage"
        ]
    );
    assert_eq!(
        session_meta_trace.fields.get("agent_id"),
        Some(&expected_trace_agent_id)
    );
    assert_eq!(
        session_meta_trace.fields.get("stage").map(String::as_str),
        Some("session_meta_touch")
    );
    assert_eq!(
        session_meta_trace
            .fields
            .get("result_class")
            .map(String::as_str),
        Some("failure")
    );
    assert_eq!(
        session_meta_trace.fields.get("message").map(String::as_str),
        Some("content-free prompt acceptance precursor")
    );
    assert!(
        session_meta_trace
            .fields
            .get("session_meta_touch_us")
            .is_some_and(|value| value.parse::<u128>().is_ok())
    );
    assert!(
        captured_traces.iter().all(|trace| !trace
            .fields
            .values()
            .any(|value| value.contains("SESSION-META-FAILURE-CANARY"))),
        "metadata failure traces must not retain prompt content"
    );
    drop(captured_traces);
    path_std_fs::remove_dir(&meta_path).expect("remove metadata obstruction");
    stale_session_manifest(&harness);
    assert!(
        event_log_events(&harness).iter().any(|event| {
            matches!(
                event,
                Event::AgentPromptSubmitted(submitted)
                    if submitted.text == "SESSION-META-FAILURE-CANARY"
            )
        }),
        "metadata failure must not retract the accepted prompt"
    );
    harness.shutdown().expect("shutdown");
}
