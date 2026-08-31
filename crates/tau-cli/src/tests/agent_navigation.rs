//! Tests for agent navigation behavior.

use super::*;

/// Agent-trace help must advertise the compact-overview semantics and both
/// default values so generated help cannot drift from parser behavior.
#[test]
fn agent_trace_help_shows_compact_toon_lite_defaults() {
    let command = path_super_cli::Cli::command();
    let mut trace = command
        .find_subcommand("agent")
        .and_then(|agent| agent.find_subcommand("trace"))
        .expect("agent trace command")
        .clone();
    let help = trace.render_long_help().to_string();

    assert!(help.contains("Project a validated durable agent snapshot"));
    assert!(help.contains("[default: agent-tools-toon]"));
    assert!(help.contains("[default: lite]"));
    assert!(help.contains("at most 4 KiB of each text/output item"));
    assert!(help.contains("complete semantic text/normalized output"));
}

#[test]
fn role_cycling_only_enabled_without_selected_agent() {
    // Regression: role cycling changes the role used for the next new agent,
    // so once an agent is selected it must stop mutating the live agent's role.
    let current_agent_state = Arc::new(Mutex::new(None));
    assert!(role_cycling_enabled(&current_agent_state));

    *current_agent_state.lock().expect("current agent") = Some(agent_id("engineer_abc12345"));
    assert!(!role_cycling_enabled(&current_agent_state));

    *current_agent_state.lock().expect("current agent") = None;
    assert!(role_cycling_enabled(&current_agent_state));
}

/// Ctrl-K/Ctrl-J cycle through active agents and the overview while skipping
/// suspended agents that would refuse user prompts.
#[test]
fn agent_switching_cycles_active_agents_and_skips_suspended() {
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned(), "charlie".to_owned()];
    let active_agents = HashSet::from([agent_id("alpha"), agent_id("charlie")]);

    assert_eq!(
        next_agent_cycle_selection(Some("alpha"), &known_agents, &active_agents, 1).as_deref(),
        Some("charlie")
    );
    assert_eq!(
        next_agent_cycle_selection(Some("alpha"), &known_agents, &active_agents, -1),
        None
    );
    assert_eq!(
        next_agent_cycle_selection(Some("charlie"), &known_agents, &active_agents, 1),
        None
    );
}

/// Cycling from the overview enters the active-agent ring from the edge
/// implied by the shortcut direction.
#[test]
fn agent_switching_without_selection_starts_at_edge_for_direction() {
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned()];
    let active_agents = HashSet::from([agent_id("alpha"), agent_id("bravo")]);

    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, 1).as_deref(),
        Some("alpha")
    );
    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, -1).as_deref(),
        Some("bravo")
    );
}

/// The overview remains the sole cycle state when no agents are active.
#[test]
fn agent_switching_without_active_agents_stays_on_overview() {
    let known_agents = vec!["suspended".to_owned()];
    let active_agents = HashSet::<tau_proto::AgentId>::new();

    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, 1),
        None
    );
    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, -1),
        None
    );
}

#[test]
fn first_agent_event_does_not_force_full_redraw() {
    // Regression: starting from the initial start-new-agent screen only changes
    // the input target. The already-visible empty transcript becomes the new
    // agent transcript in-place instead of replacing the whole output snapshot.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer_abc12345"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_abc12345"),
        ..agent_prompt_created("sp1", "s1")
    }));
    sync(&handle);
    assert_eq!(handle.full_render_count(), 0);
}

#[test]
fn new_agent_after_new_session_does_not_force_full_redraw() {
    // `:session new` intentionally moves to the start-new-agent screen and clears
    // the old transcript. Starting the next agent from that already-visible
    // empty screen should only update target/status metadata, not redraw
    // scrollback.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "first".into(),
        agent_id: tau_proto::AgentId::parse("engineer_one").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s2"),
        text: "second".into(),
        agent_id: tau_proto::AgentId::parse("engineer_two").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn new_session_initial_history_appends_to_first_agent() {
    // `:session new` can be reached after an explicit no-agent state, but the new
    // session's start screen is a fresh initial screen. Visible startup history
    // there should be adopted by the first agent instead of preserved as an
    // explicit no-agent snapshot.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("previous-agent"));
    renderer.clear_selected_agent();
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 88.into(),
        extension_name: tau_proto::ExtensionName::parse("std-session")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-session starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent(agent_id("fresh-agent"));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-session starting"));
    assert_eq!(handle.full_render_count(), full_render_count);

    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 88.into(),
        extension_name: tau_proto::ExtensionName::parse("std-session")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-session starting"));
    assert!(vt.screen_contains(80, "extension std-session ready"));
}

#[test]
fn selecting_same_agent_does_not_force_full_redraw() {
    // Regression: selecting the already-displayed target agent is a pure no-op
    // for transcript rendering.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

/// Ensures clearing an agent selection paints the no-agent transcript boundary
/// and new-agent placeholder together in the clear operation's first frame.
#[test]
fn clear_selection_first_frame_has_new_agent_placeholder() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let generation = vt.frame_generation();
    handle.with_redraw_suppressed(|| {
        renderer.switch_agent(agent_id("worker-1"));
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "selected agent transcript".into(),
            agent_id: agent_id("worker-1"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    });
    let generation = vt.wait_for_frame_containing_after(generation, "selected agent transcript");
    renderer.clear_selected_agent_after_display_update_for_test(|| handle.redraw_sync());
    let frame = vt.wait_for_frame_after(generation);

    assert!(
        !frame
            .iter()
            .any(|row| row.contains("selected agent transcript")),
        "{frame:?}"
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("Write a message to start a new agent"))
    );
    assert!(
        !frame
            .iter()
            .any(|row| row.contains("Write a message to worker-1"))
    );
}

/// Ensures transcript re-rendering retains the bounded cache estimate after
/// switching away from and back to the agent that produced the response.
#[test]
fn switching_agents_preserves_estimated_cache_efficiency() {
    // Switching away and back must retain the bounded reusable-prefix estimate
    // when durable provider usage has no exact cache-read ceiling.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent(agent_id("worker-1"));

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "first worker response",
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-1",
            "worker-1",
            20_100,
            19_000,
            0,
            "second worker response",
        ),
    ));
    renderer.switch_agent(agent_id("worker-2"));
    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);

    assert!(vt.screen_contains(80, "Δ95%? 19k/20k?"));
}

/// Ensures a hidden agent's cached response retains the same bounded estimate
/// when a later selection reconstructs that agent's transcript.
#[test]
fn switching_to_hidden_agent_preserves_estimated_cache_efficiency() {
    // Regression: hidden side-agent responses are recorded in that agent's UI
    // state and later replayed by a full transcript re-render when selected, so
    // they must retain the bounded reusable-prefix estimate.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent(agent_id("worker-1"));

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-0",
            "worker-2",
            20_000,
            0,
            0,
            "hidden first response",
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-1",
            "worker-2",
            20_100,
            19_000,
            0,
            "hidden second response",
        ),
    ));
    renderer.switch_agent(agent_id("worker-2"));
    sync(&handle);

    assert!(vt.screen_contains(80, "Δ95%? 19k/20k?"));
}

#[test]
fn extension_context_ready_routes_to_agent_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "debug");
    renderer.handle(&Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id("worker-1"),
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(80, "context ready"));

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(80, "agent @worker-1 context ready"));
}

/// The initialization UI must use real line breaks, expose only advertised
/// skill names, aggregate other skills, and put each bootstrap path on its own
/// concise line with prompt-context size statistics but without leaking skill
/// descriptions.
#[test]
fn agent_context_initialization_summary_is_concise_and_literal() {
    let advertised = tau_proto::DiscoveryEffectiveSkill {
        name: "advertised".into(),
        description: "description must stay hidden".to_owned(),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    };
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: vec![advertised],
        agents_files: vec![
            tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/home/dpc/.config/agents/AGENTS.md".into(),
                lines: 10,
                bytes: 100,
            },
            tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/repo/AGENTS.md".into(),
                lines: 20,
                bytes: 200,
            },
        ],
    };

    let block =
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, 2);
    let spans = block.content.spans();
    let text = spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        "▤ initialized agent-1\nskills:\n  advertised 1L, 28B\n  2 other session skills available\nAGENTS.md:\n  /home/dpc/.config/agents/AGENTS.md 10L, 100B\n  /repo/AGENTS.md 20L, 200B"
    );
    assert!(!text.contains("\\n"));
    assert!(!text.contains("description must stay hidden"));
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::DarkCyan))
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        [" 1L, 28B", " 10L, 100B", " 20L, 200B"]
    );
}

/// Empty sections stay omitted, while a singular aggregate remains grammatical.
#[test]
fn agent_context_initialization_summary_omits_empty_sections() {
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: Vec::new(),
        agents_files: Vec::new(),
    };
    let text = |count| {
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, count)
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>()
    };

    assert_eq!(text(0), "▤ initialized agent-1");
    assert_eq!(
        text(1),
        "▤ initialized agent-1\nskills:\n  1 other session skill available"
    );
}

/// The event renderer must combine the canonical session snapshot with the
/// agent-specific prompt projection in the observable terminal summary.
#[test]
fn agent_context_initialization_event_aggregates_session_skills() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let skill = |name: &str| tau_proto::DiscoveryEffectiveSkill {
        name: name.into(),
        description: format!("{name} description"),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    };
    renderer.handle(&Event::HarnessSessionSkillsAvailable(
        tau_proto::HarnessSessionSkillsAvailable {
            session_id: test_session_id("session-1"),
            skills: vec![skill("advertised"), skill("other")],
        },
    ));
    renderer.switch_agent(agent_id("agent-1"));
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id("agent-1"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
            listed_skills: vec![skill("advertised")],
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/repo/AGENTS.md".into(),
                lines: 20,
                bytes: 200,
            }],
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "initialized agent-1"));
    assert!(vt.screen_contains(80, "advertised"));
    assert!(vt.screen_contains(80, "1 other session skill available"));
    assert!(vt.screen_contains(80, "/repo/AGENTS.md"));
    assert!(vt.screen_contains(80, "advertised 1L, 22B"));
    assert!(vt.screen_contains(80, "/repo/AGENTS.md 20L, 200B"));
    assert!(!vt.screen_contains(80, "other description"));
}

/// Current-state discovery catch-up received before any selection must remain
/// hidden until the user selects its owning agent, as on restore or reattach.
#[test]
fn catch_up_agent_context_initialization_waits_for_agent_selection() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "debug");
    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "startup".into(),
        message: "startup output adopted by first agent".into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id("restored"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("restored-init")
                .expect("test identifier must be valid"),
            listed_skills: Vec::new(),
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/restored/AGENTS.md".into(),
                lines: 6,
                bytes: 550,
            }],
        },
    ));
    renderer.handle(&Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("restored-init")
                .expect("test identifier must be valid"),
            session_id: test_session_id("session-1"),
            agent_id: agent_id("restored"),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(!vt.screen_contains(80, "/restored/AGENTS.md"));
    assert!(!vt.screen_contains(80, "context ready"));

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("restored"),
        ..agent_prompt_created("restored-prompt", "session-1")
    }));
    sync(&handle);
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("restored")
    );
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(vt.screen_contains(80, "/restored/AGENTS.md 6L, 550B"));
    assert!(vt.screen_contains(80, "context ready"));
    let restored = vt.screen_text(80).join("\n");
    assert!(
        restored.find("/restored/AGENTS.md").expect("discovery row")
            < restored
                .find("context ready")
                .expect("later context-ready row")
    );

    renderer.switch_agent(agent_id("background"));
    sync(&handle);
    assert!(!vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(!vt.screen_contains(80, "/restored/AGENTS.md"));

    renderer.switch_agent(agent_id("restored"));
    sync(&handle);
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(vt.screen_contains(80, "/restored/AGENTS.md 6L, 550B"));
}

/// Extension lifecycle blocks that start on the initial no-agent screen are
/// part of the first agent conversation and must not be cleared by selecting
/// that first agent. This protects the startup/agent-selection flow from
/// redrawing away visible history before the conversation has really begun.
#[test]
fn initial_no_agent_extension_lifecycle_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent(agent_id("fresh-agent"));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));
    assert_eq!(handle.full_render_count(), full_render_count);

    renderer.handle(&Event::ExtensionExited(tau_proto::ExtensionExited {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
        exit_code: Some(1),
        signal: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global starting"));
    assert!(vt.screen_contains(80, "extension std-global exited"));
}

/// Extension lifecycle blocks on an explicitly cleared no-agent screen must
/// stay owned by that global snapshot. That state is different from process
/// startup: the user intentionally left an existing agent transcript, so fresh
/// agents should not inherit global no-agent output.
#[test]
fn explicit_no_agent_extension_lifecycle_routes_to_no_agent_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("previous-agent"));
    renderer.clear_selected_agent();
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));

    renderer.switch_agent(agent_id("fresh-agent"));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global starting"));

    renderer.handle(&Event::ExtensionExited(tau_proto::ExtensionExited {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
        exit_code: Some(1),
        signal: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global exited"));
    assert!(!vt.screen_contains(80, "extension std-global starting"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global exited"));
    assert!(!vt.screen_contains(80, "extension std-global starting"));
}

/// A replayed global fact on the initial no-agent screen remains owned by that
/// screen instead of being adopted into the first fresh agent transcript.
#[test]
fn initial_replayed_global_message_fact_survives_first_agent_selection() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("unavailable-agent"),
        tau_proto::MessageFactId::new("replayed-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "initial replay body",
    ));
    let delivery = tau_proto::EventDelivery::replay(tau_proto::UnixMicros::new(1_000_000), fact);
    let (event, replay, recorded_at) = delivery.into_parts();
    assert!(replay);
    renderer.handle_recorded_at(&event, recorded_at.expect("replay timestamp"));
    sync(&handle);
    assert!(vt.screen_contains(100, "initial replay body"));

    renderer.switch_agent(agent_id("first-fresh-agent"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "initial replay body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "initial replay body"));
}

/// A live global fact received after explicit deselection stays in the
/// no-agent snapshot when the user switches to a never-cached agent.
#[test]
fn deselected_live_global_message_fact_survives_fresh_agent_selection() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("existing-agent"));
    renderer.clear_selected_agent();
    renderer.handle(&Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("unavailable-agent"),
        tau_proto::MessageFactId::new("live-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "deselected live body",
    )));
    sync(&handle);
    assert!(vt.screen_contains(100, "deselected live body"));

    renderer.switch_agent(agent_id("never-cached-agent"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "deselected live body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "deselected live body"));
}

/// Dynamic action errors invoked from the initial no-agent screen are adopted
/// by the first selected agent, matching successful action output and startup
/// extension status.
#[test]
fn initial_no_agent_action_error_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.switch_agent(agent_id("fresh-agent"));
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        message: "no-agent action failed".to_owned(),
        details: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "no-agent action failed"));
}

/// Dynamic action errors invoked after explicit deselection must not appear in
/// a later-selected agent transcript. This preserves the global/no-agent
/// snapshot boundary for extension action failures just like successful output.
#[test]
fn explicit_no_agent_action_error_routes_to_no_agent_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("previous-agent"));
    renderer.clear_selected_agent();
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.switch_agent(agent_id("fresh-agent"));
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        message: "no-agent action failed".to_owned(),
        details: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "no-agent action failed"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "no-agent action failed"));
}

/// No-agent action output that arrives on the initial start-new-agent screen is
/// part of the first agent conversation and should remain visible when that
/// agent is selected.
#[test]
fn initial_no_agent_action_result_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "visible no-agent action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));

    renderer.switch_agent(agent_id("fresh-agent"));
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));
}

/// No-agent action output that arrives after explicit deselection must be
/// snapshotted before switching to a fresh agent. Otherwise the fresh agent
/// would inherit global action output that was never scoped to it.
#[test]
fn explicit_no_agent_action_result_is_preserved_when_switching_to_fresh_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("previous-agent"));
    renderer.clear_selected_agent();
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "visible no-agent action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));

    renderer.switch_agent(agent_id("fresh-agent"));
    sync(&handle);
    assert!(!vt.screen_contains(80, "visible no-agent action output"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));
}

#[test]
fn hidden_agent_events_do_not_force_visible_full_redraw() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    sync(&handle);
    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn agent_stats_does_not_overwrite_display_name() {
    // `:agent switch` completions are backed by durable display names. Agent stats
    // must not replace the display name chosen by the harness template.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-Ab12"),
        role: "engineer".to_owned(),
        display_name: Some("engineer: look it up".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer-Ab12"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    let display_names = renderer.agent_display_names();
    let display_names = display_names.lock().expect("display names");
    assert_eq!(
        display_names.get("engineer-Ab12").map(String::as_str),
        Some("engineer: look it up")
    );
}

/// Ensures requester acknowledgements and diagnostics never become cache
/// authority; only a subsequent complete stats snapshot changes navigation.
#[test]
fn navigation_mode_results_do_not_mutate_cache() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Suspended);
    for outcome in [
        tau_proto::UiSetAgentNavigationModeOutcome::Applied,
        tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
            reason: tau_proto::UiSetAgentNavigationModeRejection::StaleSession,
        },
        tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
            reason: tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded,
        },
    ] {
        renderer.handle(&Event::UiSetAgentNavigationModeResult(
            tau_proto::UiSetAgentNavigationModeResult {
                request_id: "result".to_owned(),
                session_id: test_session_id("s1"),
                agent_id: agent_id("worker-1"),
                outcome,
            },
        ));
        assert_eq!(
            renderer
                .agent_navigation()
                .lock()
                .expect("navigation")
                .mode(&agent_id("worker-1")),
            tau_proto::AgentNavigationMode::Suspended
        );
    }
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("navigation")
            .mode(&agent_id("worker-1")),
        tau_proto::AgentNavigationMode::Active
    );
}

/// Ensures placeholder copy distinguishes idle automatic hiding from an
/// unconditional manual suspension.
#[test]
fn selected_hidden_agent_placeholder_distinguishes_modes() {
    // Hidden selected agents remain viewable, and copy names the explicit
    // transition.
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(100, "active-auto agent is idle"));
    // Exercise the placeholder-only navigation path: the operation must request
    // its own redraw even when no model-status block is present to do so.
    renderer.clear_model_status_for_test();
    sync(&handle);
    let generation = vt.frame_generation();
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Suspended);
    let generation = vt.wait_for_frame_containing_after(generation, "This agent is suspended");
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    vt.wait_for_frame_containing_after(generation, "Write a message to worker-1");
}

/// Ensures a delayed renderer refresh after unload cannot resurrect membership
/// or attach an override to a later same-id delegated endpoint.
#[test]
fn delayed_navigation_refresh_cannot_resurrect_unloaded_agent() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker-1".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    renderer.handle(&Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker-1"),
        },
    ));

    assert!(
        !renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .is_live(&agent_id("worker-1"))
    );

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker-2".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(
        navigation.mode(&agent_id("worker-1")),
        AgentNavigationState::Active
    );
    assert!(!navigation.is_active(&agent_id("worker-1")));
}

#[test]
fn hidden_agent_activity_keeps_global_in_progress() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..finished_response("worker-sp", vec![assistant_message_item("done")])
    }));

    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

/// Ensures a watched agent's stats stay owned by its watcher even while that
/// watcher is not the selected transcript, preventing a hidden-row redraw from
/// leaking into another agent view.
#[test]
fn watched_agent_stats_route_to_hidden_watcher_owner() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("worker-1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_started("ap-engineer_1-0", "s1")
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 1,
            started_total: 2,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(90, "❓💤 @engineer_1"));

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(90, "❓💤 @engineer_1"));
    assert!(vt.screen_contains(90, "%1/2"));
}

/// `:new` leaves the old agent running while the terminal shows the all-agent
/// overview. Its messages must appear there without selecting the sender, while
/// also remaining available in the sender's own transcript.
#[test]
fn old_agent_message_updates_overview_without_selecting_sender() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old agent prompt"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old agent prompt"));

    renderer.handle(&agent_message(
        "old-agent",
        "other-agent",
        "hidden old-agent message",
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from @old-agent to @other-agent"));
    assert!(vt.screen_contains(80, "hidden old-agent message"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent(agent_id("old-agent"));
    sync(&handle);
    assert!(vt.screen_contains(80, "hidden old-agent message"));
}

#[test]
fn manual_compaction_selects_agent_from_empty_state() {
    // Regression: replay can expose a user-triggered compaction before any
    // prompt-created/submitted event. Even though manual compaction is not
    // rendered as progress, it still identifies the agent the empty UI should
    // target for subsequent input.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentCompactionTriggered(AgentCompactionTriggered {
        agent_id: tau_proto::AgentId::parse("live-agent").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
        resume_inference: false,
    }));
    sync(&handle);

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some(agent_id("live-agent"))
    );
}

/// Role-update parsing must keep explicit `off` distinct from clearing a field;
/// otherwise `:role <role> effort off` and `:role <role> thinking-summary off`
/// would accidentally reset the selected role instead of storing the user's
/// requested off state. `reset` is the only textual way to clear a setting.
#[test]
fn role_setting_updates_are_typed_and_reset_aware() {
    use std::num::NonZeroU8;

    use super::super::ui_commands::parse_role_setting_update;

    let tool_names = || {
        vec![
            tau_proto::ToolName::new("web_search"),
            tau_proto::ToolName::new("grep"),
        ]
    };
    let tool_group_names = || {
        vec![
            tau_proto::ToolGroupName::new("web"),
            tau_proto::ToolGroupName::new("shell"),
        ]
    };

    for (setting, value, expected) in [
        (
            "model",
            "openai/gpt-4o",
            UiRoleUpdateAction::SetModel {
                model: Some("openai/gpt-4o".parse().expect("valid model id")),
            },
        ),
        (
            "effort",
            "off",
            UiRoleUpdateAction::SetEffort {
                effort: Some(Effort::Off),
            },
        ),
        (
            "effort",
            "reset",
            UiRoleUpdateAction::SetEffort { effort: None },
        ),
        (
            "effort",
            "increase:2",
            UiRoleUpdateAction::AdjustEffort {
                adjustment: tau_proto::UiRoleSettingAdjustment::Increase(
                    NonZeroU8::new(2).expect("positive"),
                ),
            },
        ),
        (
            "verbosity",
            "high",
            UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
            },
        ),
        (
            "verbosity",
            "decrease",
            UiRoleUpdateAction::AdjustVerbosity {
                adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(
                    NonZeroU8::new(1).expect("positive"),
                ),
            },
        ),
        (
            "thinking-summary",
            "off",
            UiRoleUpdateAction::SetThinkingSummary {
                thinking_summary: Some(ThinkingSummary::Off),
            },
        ),
        (
            "thinking-summary",
            "increase:3",
            UiRoleUpdateAction::AdjustThinkingSummary {
                adjustment: tau_proto::UiRoleSettingAdjustment::Increase(
                    NonZeroU8::new(3).expect("positive"),
                ),
            },
        ),
        (
            "service-tier",
            "fast",
            UiRoleUpdateAction::SetServiceTier {
                service_tier: Some(ServiceTier::Fast),
            },
        ),
        (
            "service-tier",
            "reset",
            UiRoleUpdateAction::SetServiceTier { service_tier: None },
        ),
        (
            "compaction-threshold",
            "85000",
            UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold: Some(tau_proto::TokenCount::new(85000)),
            },
        ),
        (
            "compaction-threshold",
            "reset",
            UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold: None,
            },
        ),
        (
            "tools",
            "web_search,grep",
            UiRoleUpdateAction::SetTools {
                tools: Some(tool_names()),
            },
        ),
        (
            "tools",
            "reset",
            UiRoleUpdateAction::SetTools { tools: None },
        ),
        (
            "enable-tool-groups",
            "web,shell",
            UiRoleUpdateAction::SetEnableToolGroups {
                enable_tool_groups: tool_group_names(),
            },
        ),
        (
            "disable-tool-groups",
            "web,shell",
            UiRoleUpdateAction::SetDisableToolGroups {
                disable_tool_groups: tool_group_names(),
            },
        ),
        (
            "enable-tools",
            "web_search,grep",
            UiRoleUpdateAction::SetEnableTools {
                enable_tools: tool_names(),
            },
        ),
        (
            "enable-tools",
            "reset",
            UiRoleUpdateAction::SetEnableTools {
                enable_tools: Vec::new(),
            },
        ),
        (
            "disable-tools",
            "web_search,grep",
            UiRoleUpdateAction::SetDisableTools {
                disable_tools: tool_names(),
            },
        ),
        (
            "disable-tools",
            "reset",
            UiRoleUpdateAction::SetDisableTools {
                disable_tools: Vec::new(),
            },
        ),
    ] {
        assert_eq!(
            parse_role_setting_update(setting, value).expect("role setting parses"),
            expected,
            "{setting} {value}"
        );
    }

    assert!(parse_role_setting_update("service-tier", "off").is_err());
    assert!(parse_role_setting_update("compaction-threshold", "999").is_err());
    assert_eq!(
        parse_role_setting_update("unknown", "value").expect_err("unknown setting"),
        "unknown setting"
    );
}

/// The no-agent screen aggregates one entry per semantic inter-agent message,
/// while sender and recipient transcripts retain their own projections.
/// Starting a new agent from the overview must not adopt that aggregate
/// history.
#[test]
fn no_agent_overview_deduplicates_agent_message_projections() {
    let (_term, handle, vt) = setup(96, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("sender-agent"));

    renderer.handle(&agent_message(
        "sender-agent",
        "recipient-agent",
        "overview semantic body",
    ));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-sender-agent-recipient-agent")
                .expect("test message id must satisfy its grammar"),
            sender_id: agent_id("sender-agent"),
            sender_session_id: None,
            recipient_id: agent_id("recipient-agent"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "overview semantic body".to_owned(),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert_eq!(
        visible_lines(&vt, 96)
            .iter()
            .filter(|line| line.contains("overview semantic body"))
            .count(),
        1
    );
    renderer.handle(&agent_message(
        "sender-agent",
        "third-agent",
        "live overview body",
    ));
    sync(&handle);
    assert!(vt.screen_contains(96, "live overview body"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent(agent_id("recipient-agent"));
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "start fresh from overview".to_owned(),
        agent_id: agent_id("fresh-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(96, "start fresh from overview"));
    assert!(!vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));
}

/// Hidden agent and no-agent transcript snapshots reproject current display
/// names when selected again instead of retaining event-time labels.
#[test]
fn hidden_message_snapshots_reproject_late_agent_names() {
    let (_term, handle, vt) = setup(100, 10);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");

    renderer.switch_agent(agent_id("agent-a"));
    renderer.handle(&agent_message("agent-a", "agent-b", "agent history"));
    renderer.switch_agent(agent_id("viewer"));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "late worker".to_owned(),
        },
    ));
    sync(&handle);
    let generation = vt.frame_generation();
    renderer.switch_agent(agent_id("agent-a"));
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a to @agent-b (late worker):",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (late worker):"));

    renderer.clear_selected_agent();
    renderer.handle(&agent_message(
        "agent-a",
        "agent-c",
        "overview message history",
    ));
    renderer.switch_agent(agent_id("viewer"));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-a"),
            display_name: "late sender".to_owned(),
        },
    ));
    sync(&handle);
    let generation = vt.frame_generation();
    renderer.clear_selected_agent();
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a (late sender) to @agent-c:",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a (late sender) to @agent-c:"));
    assert!(vt.screen_contains(100, "overview message history"));
}

/// A new session discards compact-hidden no-agent notices, so toggling verbose
/// later cannot revive transcript state from the previous session.
#[test]
fn new_session_discards_compact_hidden_no_agent_notices() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice::diagnostic(
        "test.info",
        "old no-agent notice",
        tau_proto::NoticeLevel::Info,
    )));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old no-agent notice"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("new-session"),
        reason: SessionStartReason::New,
    }));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old no-agent notice"));
}

/// The production lightweight prompt lifecycle supplies the selected agent's
/// model and repaints pacing whether quota catch-up arrives before or after it.
#[test]
fn selected_agent_quota_repaints_for_both_event_orderings() {
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    for quota_first in [true, false] {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("other/model".into()),
            context_window: None,
            role: "engineer".into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        renderer.handle(&Event::SessionStarted(SessionStarted {
            session_id: test_session_id("quota-order"),
            reason: SessionStartReason::Initial,
        }));
        let started = Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model: model.clone(),
            ..agent_prompt_started("quota-sp", "quota-order")
        });
        let quota = danger_quota_event(&model);

        if quota_first {
            renderer.handle(&quota);
            sync(&handle);
            assert!(!vt.screen_contains(80, "Q!"));
            renderer.handle(&started);
        } else {
            renderer.handle(&started);
            sync(&handle);
            assert!(!vt.screen_contains(80, "Q!"));
            renderer.handle(&quota);
        }
        sync(&handle);

        assert!(
            vt.screen_contains(80, "Q!"),
            "selected-agent quota was not repainted when quota_first={quota_first}"
        );
    }
}

#[test]
fn focused_agent_context_usage_event_replaces_unknown_context_window() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            input_tokens: Some(12_000),
            cached_tokens: Some(0),
            context_window: Some(200_000),
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row");
    assert!(status_row.ends_with("#12k/200k -/-"));
    assert!(!status_row.contains("#-/200k"));
}

#[test]
fn role_default_knobs_are_hidden_and_overrides_follow_role() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=medium, verbosity=medium, thinking-summary=auto"
                .to_owned(),
            role_description: None,
            details: None,
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(!vt.screen_contains(80, "^medium"));
    assert!(!vt.screen_contains(80, "~medium"));

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ~high"));
}

#[test]
fn role_state_overrides_are_compared_to_role_baseline() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // HarnessRolesAvailable describes the current role including
    // persisted state overrides. The status bar must use the role/provider
    // baseline from HarnessRoleSelected instead.
    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=low, verbosity=high, thinking-summary=auto"
                .to_owned(),
            role_description: None,
            details: None,
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: None,
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Low,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: Some(tau_proto::ServiceTier::Fast),
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s3"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ^low ~high !off"));
}

/// A pending agent response must remain above watched-agent status rows whether
/// the response or watch snapshot arrives first.
#[test]
fn pending_agent_response_stays_above_watched_agent_rows() {
    for response_arrives_first in [true, false] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.switch_agent(agent_id("main"));

        let render_response = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
                "sp-pending",
                "s1",
            )));
            renderer.handle(&Event::ProviderResponseUpdated(
                provider_response_delta_update(
                    test_agent_prompt_id("sp-pending"),
                    "pending agent response",
                    None,
                    tau_proto::PromptOriginator::User,
                ),
            ));
        };
        let render_watch = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentWatchesUpdated(
                tau_proto::AgentWatchesUpdated {
                    session_id: test_session_id("s1"),
                    watcher_id: agent_id("main"),
                    watched_agent_ids: vec![agent_id("engineer_1")],
                    changed_agent_id: Some(agent_id("engineer_1")),
                    cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
                },
            ));
        };

        if response_arrives_first {
            render_response(&mut renderer);
            render_watch(&mut renderer);
        } else {
            render_watch(&mut renderer);
            render_response(&mut renderer);
        }
        sync(&handle);

        let screen = vt.screen_text(100);
        let response = screen
            .iter()
            .position(|line| line.contains("pending agent response"))
            .unwrap_or_else(|| panic!("missing pending response in {screen:?}"));
        let watched = screen
            .iter()
            .position(|line| line.contains("❓💤 @engineer_1"))
            .unwrap_or_else(|| panic!("missing watched row in {screen:?}"));
        assert!(
            response < watched,
            "pending response should stay above watched rows: {screen:?}"
        );
    }
}

/// Recursive watched rows must include topology-only descendants before any
/// stats arrive, label their deterministic parent, and retain distinct
/// descendant-witness activity on direct rows.
///
/// This prevents a parent row from flickering out between child model rounds or
/// losing the reason an indirect child appears in the selected transcript.
#[test]
fn watched_agent_recursive_rows_keep_via_and_distinct_witness_context() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent(agent_id("manager"));
    for (watcher, watched) in [("manager", "reviewer"), ("reviewer", "worker")] {
        renderer.handle(&Event::AgentWatchesUpdated(
            tau_proto::AgentWatchesUpdated {
                session_id: test_session_id("s1"),
                watcher_id: agent_id(watcher),
                watched_agent_ids: vec![agent_id(watched)],
                changed_agent_id: Some(agent_id(watched)),
                cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
            },
        ));
    }
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @reviewer"));
    assert!(
        vt.screen_contains(100, "❓💤 @worker via @reviewer"),
        "topology-only indirect rows must remain visible without stats"
    );
    let prompt_started = |agent: &str| {
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            session_id: test_session_id("s1"),
            agent_id: agent_id(agent),
            agent_prompt_id: test_agent_prompt_id(format!("ap-{agent}")),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })
    };

    renderer.handle(&prompt_started("worker"));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @reviewer watching -> @worker"));
    assert!(
        vt.screen_contains(100, "❓✨ @worker via @reviewer"),
        "via context and direct running state must coexist"
    );

    renderer.handle(&prompt_started("reviewer"));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓✨ @reviewer"));
    assert!(vt.screen_contains(100, "❓✨ @worker via @reviewer"));
    assert!(
        !vt.screen_contains(100, "@reviewer -> @worker"),
        "direct-running state must replace the transitive witness"
    );
}

/// Only a complete syntactically valid agent reference receives the agent-id
/// priority; arbitrary free-form `@` chips retain generic-info semantics.
#[test]
fn info_chip_agent_classification_is_strict() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            info_chips: vec!["@engineer_child".into(), "@not an agent".into()],
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );

    assert!(matches!(display.suffixes[0].status, ToolStatus::Agent));
    assert!(matches!(display.suffixes[1].status, ToolStatus::Info));
}

/// Indirect-row context styles its parent ID as a watched-agent identity while
/// retaining the `via` label's context style. It also follows stable identity
/// at practical widths, while narrow layouts retain the fixed work-status
/// prefix and a distinguishable identity within the exact terminal budget.
#[test]
fn watched_agent_indirect_context_respects_width_priorities() {
    let theme = cli_test_theme();
    let status = tau_proto::AgentWatchWorkStatusNotification {
        session_id: test_session_id("s1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
        phase: tau_proto::AgentWorkStatusPhase::Working,
        title: Some("review changes".to_owned()),
        initial: false,
    };
    let stats = tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-with-long-id"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 1,
        },
        context: tau_proto::AgentContextStats {
            input_tokens: None,
            cached_tokens: None,
            context_window: None,
            percent_used: Some(67),
        },
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    let display = watched_agent_tool_display(
        None,
        "worker-with-long-id",
        Some("reviewer"),
        Some(&stats),
        WatchedAgentActivity::Idle,
        Some(&status),
    );
    let block = render_tool_block(&theme, &display);

    assert_eq!(
        priority_header_text(&block, 100),
        "🚀💤 @worker-with-long-id via @reviewer review changes %1/1 #67%"
    );
    let cells = priority_header_cells(&block, 100);
    let parent_identity_start = cells
        .iter()
        .rposition(|cell| cell.ch == '@')
        .expect("recursive watch parent identity");
    assert_eq!(
        cells[parent_identity_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME),
        "recursive watch parent identity uses the watched-agent identity style"
    );
    let via_start = cells
        .iter()
        .position(|cell| cell.ch == 'v')
        .expect("recursive watch context label");
    assert_eq!(
        cells[via_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "recursive watch context label remains agent-context metadata"
    );
    let boundary = priority_header_text(&block, 39);
    assert!(boundary.contains("via @reviewer"), "{boundary:?}");
    assert!(boundary.starts_with("🚀💤 @"), "{boundary:?}");
    for width in [12, 16, 24, 40] {
        let text = priority_header_text(&block, width);
        assert!(text.starts_with("🚀💤 @"), "{width}: {text:?}");
        assert!(
            tau_term_screen::display_width(&text) <= width,
            "{width}: {text:?}"
        );
    }
}
