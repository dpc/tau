use std::collections as path_std_collections;
use std::sync::atomic as path_std_sync_atomic;

use super::*;
fn routing_state(
    known: Arc<Mutex<Vec<String>>>,
    live: Arc<Mutex<std::collections::HashSet<String>>>,
    suspended: Arc<Mutex<std::collections::HashSet<String>>>,
) -> InputRoutingState {
    let mut navigation = AgentNavigation::default();
    for agent_id in live.lock().expect("live agents lock").iter() {
        let agent_id = tau_proto::AgentId::parse(agent_id).expect("valid agent id");
        navigation.mark_live(agent_id.clone());
        navigation.apply_stats(
            &agent_id,
            tau_proto::AgentNavigationMode::Active,
            tau_proto::AgentRuntimeState::Idle,
        );
    }
    for agent_id in suspended.lock().expect("suspended agents lock").iter() {
        let agent_id = tau_proto::AgentId::parse(agent_id).expect("valid agent id");
        navigation.apply_stats(
            &agent_id,
            tau_proto::AgentNavigationMode::Suspended,
            tau_proto::AgentRuntimeState::Idle,
        );
    }
    InputRoutingState::new(
        Arc::new(Mutex::new(None)),
        known,
        Arc::new(Mutex::new(navigation)),
        Arc::new(Mutex::new(path_std_collections::HashSet::new())),
    )
}

#[test]
fn agent_completer_offers_subcommands_first() {
    // `:agent` is now a command group; the first argument must guide users
    // to the concrete action instead of switching immediately.
    let completer = build_agent_arg_completer(
        routing_state(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Default::default())),
            Arc::new(Mutex::new(Default::default())),
        ),
        Arc::new(Mutex::new(HashMap::new())),
    );

    let completions = completer(&[""]);

    let entries: Vec<_> = completions
        .iter()
        .map(|item| (item.value.as_str(), item.description.as_str()))
        .collect();
    assert_eq!(
        entries,
        vec![
            ("new", "Clear the selected agent"),
            ("switch", "Show a known agent transcript"),
            ("suspend", "Exclude an active agent from navigation"),
            ("resume", "Make a loaded agent always navigation-eligible"),
            ("auto", "Make a loaded agent eligible only while running"),
            ("name", "Set an agent display name"),
        ]
    );
}

#[test]
fn session_completer_offers_new_subcommand() {
    // `:session new` is the session-level fresh-start command; `:new` is
    // reserved as an alias for `:agent new`.
    let completer = build_session_arg_completer();

    let entries: Vec<_> = completer(&[""])
        .into_iter()
        .map(|item| (item.value, item.description))
        .collect();

    assert_eq!(
        entries,
        vec![("new".to_owned(), "Start a fresh chat session".to_owned())]
    );
}

#[test]
fn agent_new_takes_no_agent_id_completion() {
    // `:agent new` only clears the selected agent; it must not offer or
    // accept an agent-id argument like switch/suspend/resume do.
    let completer = build_agent_arg_completer(
        routing_state(
            Arc::new(Mutex::new(vec!["worker".to_owned()])),
            Arc::new(Mutex::new(path_std_collections::HashSet::from([
                "worker".to_owned()
            ]))),
            Arc::new(Mutex::new(Default::default())),
        ),
        Arc::new(Mutex::new(HashMap::new())),
    );

    assert!(completer(&["new", ""]).is_empty());
}

/// Ensures `:suspend` resolves the selected target without optimistically
/// mutating the authoritative cache.
#[test]
fn selected_agent_suspend_alias_dispatches_existing_suspend_flow() {
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::new()));
    let routing = routing_state(known, live.clone(), suspended.clone());
    routing.set_selected_agent(Some(agent_id("worker")));
    let messages = Arc::new(Mutex::new(Vec::new()));

    let target = handle_agent_suspend_command(&routing, None, &|message| {
        messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.to_owned());
    });

    assert!(messages.lock().expect("messages lock poisoned").is_empty());
    assert_eq!(target.as_deref(), Some("worker"));
    assert!(routing.agent_is_active(&agent_id("worker")));
}

/// Ensures `:resume` resolves the selected suspended agent without changing the
/// cache before the harness snapshot arrives.
#[test]
fn selected_agent_resume_alias_dispatches_existing_resume_flow() {
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "worker".to_owned()
    ])));
    let routing = routing_state(known, live.clone(), suspended.clone());
    routing.set_selected_agent(Some(agent_id("worker")));
    let messages = Arc::new(Mutex::new(Vec::new()));

    let target = handle_agent_resume_command(&routing, None, &|message| {
        messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.to_owned());
    });

    assert!(messages.lock().expect("messages lock poisoned").is_empty());
    assert_eq!(target.as_deref(), Some("worker"));
    assert!(!routing.agent_is_active(&agent_id("worker")));
}

#[test]
fn agent_mention_completer_offers_only_active_agents() {
    // Prompt-text `@agent` completion is for routing to active agents. It
    // must not suggest suspended agents even though `:agent resume` does.
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned()
    ])));
    let completer = build_agent_mention_completer(routing_state(known, live, suspended));

    let values: Vec<_> = completer(&[""])
        .into_iter()
        .map(|item| item.value)
        .collect();

    assert_eq!(values, vec!["worker"]);
}

/// Ensures all prompt-routing completions consume the same effective set as an
/// `active-auto` agent crosses authoritative running and idle edges.
#[test]
fn active_auto_completion_follows_runtime_state() {
    let mut navigation = AgentNavigation::default();
    let helper_id = tau_proto::AgentId::parse("helper").expect("valid agent id");
    navigation.mark_live(helper_id.clone());
    navigation.apply_stats(
        &helper_id,
        tau_proto::AgentNavigationMode::ActiveAuto,
        tau_proto::AgentRuntimeState::Idle,
    );
    let navigation = Arc::new(Mutex::new(navigation));
    let routing = InputRoutingState::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(vec!["helper".to_owned()])),
        navigation.clone(),
        Arc::new(Mutex::new(Default::default())),
    );
    let mentions = build_agent_mention_completer(routing.clone());
    let agents = build_agent_arg_completer(routing.clone(), Arc::new(Mutex::new(HashMap::new())));

    assert!(mentions(&[""]).is_empty());
    assert_eq!(agents(&["resume", ""])[0].value, "helper");

    navigation.lock().expect("agent navigation").apply_stats(
        &helper_id,
        tau_proto::AgentNavigationMode::ActiveAuto,
        tau_proto::AgentRuntimeState::Running,
    );
    assert_eq!(mentions(&[""])[0].value, "helper");
    assert_eq!(agents(&["switch", ""])[1].value, "helper");
    assert_eq!(agents(&["suspend", ""])[0].value, "helper");
    assert!(agents(&["resume", ""]).is_empty());

    navigation.lock().expect("agent navigation").apply_stats(
        &helper_id,
        tau_proto::AgentNavigationMode::ActiveAuto,
        tau_proto::AgentRuntimeState::Idle,
    );
    assert!(mentions(&[""]).is_empty());
    assert_eq!(agents(&["resume", ""])[0].value, "helper");
}

#[test]
fn agent_completer_filters_active_and_suspended_agents() {
    // Suspended delegate agents should disappear from switch/suspend menus so
    // tab completion stays focused on active choices, but remain available for
    // explicit resume.
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned()
    ])));
    let completer = build_agent_arg_completer(
        routing_state(known, live, suspended),
        Arc::new(Mutex::new(HashMap::new())),
    );

    let switch_values: Vec<_> = completer(&["switch", ""])
        .into_iter()
        .map(|item| item.value)
        .collect();
    let suspend_values: Vec<_> = completer(&["suspend", ""])
        .into_iter()
        .map(|item| item.value)
        .collect();
    let resume_values: Vec<_> = completer(&["resume", ""])
        .into_iter()
        .map(|item| item.value)
        .collect();

    assert_eq!(switch_values, vec!["none", "worker"]);
    assert_eq!(suspend_values, vec!["worker"]);
    assert_eq!(resume_values, vec!["helper"]);
}

/// Complete explicit-agent commands normalize one optional `@` before their
/// command-specific effect, as specified by SPEC-tau-cli-command-mode.
#[test]
fn agent_commands_accept_prefixed_references_in_canonical_effects() {
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned()
    ])));
    let routing = routing_state(known, live, suspended);

    for command in [":agent switch helper", ":agent switch @helper"] {
        assert_eq!(
            agent_command_effect(command, &routing).expect("switch effect"),
            AgentCommandEffect::Switch(Some(agent_id("helper")))
        );
    }
    for command in [":agent suspend worker", ":agent suspend @worker"] {
        assert_eq!(
            agent_command_effect(command, &routing).expect("suspend effect"),
            AgentCommandEffect::SetNavigation {
                agent_id: tau_proto::AgentId::parse("worker").expect("agent id"),
                action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
            }
        );
    }
    for command in [":agent resume helper", ":agent resume @helper"] {
        assert_eq!(
            agent_command_effect(command, &routing).expect("resume effect"),
            AgentCommandEffect::SetNavigation {
                agent_id: tau_proto::AgentId::parse("helper").expect("agent id"),
                action: tau_proto::UiAgentNavigationModeAction::SetActive,
            }
        );
    }
    for command in [":agent auto worker", ":agent auto @worker"] {
        assert_eq!(
            agent_command_effect(command, &routing).expect("auto effect"),
            AgentCommandEffect::SetNavigation {
                agent_id: tau_proto::AgentId::parse("worker").expect("agent id"),
                action: tau_proto::UiAgentNavigationModeAction::SetActiveAuto,
            }
        );
    }
    for command in [
        ":agent name worker Worker name",
        ":agent name @worker Worker name",
        ":agent name\t@worker Worker name",
    ] {
        assert_eq!(
            agent_command_effect(command, &routing).expect("name effect"),
            AgentCommandEffect::SetDisplayName(AgentDisplayNameRequest {
                agent_id: tau_proto::AgentId::parse("worker").expect("agent id"),
                display_name: "Worker name".to_owned(),
            })
        );
    }
}

/// Malformed prefixed references fail at the complete command/effect boundary,
/// so the input loop can emit the error without applying any renderer or
/// protocol effect.
#[test]
fn agent_commands_reject_malformed_prefixed_references() {
    let routing = routing_state(
        Arc::new(Mutex::new(vec!["worker".to_owned()])),
        Arc::new(Mutex::new(path_std_collections::HashSet::from([
            "worker".to_owned()
        ]))),
        Arc::new(Mutex::new(Default::default())),
    );

    for command in [
        ":agent switch @",
        ":agent suspend @@worker",
        ":agent resume @bad/id",
        ":agent auto @",
        ":agent name @@worker Worker",
    ] {
        let error = agent_command_effect(command, &routing).expect_err("command must be rejected");
        assert!(error.starts_with("invalid agent id `@"), "{error}");
    }
}

/// Previous/next application navigation updates the input target and publishes
/// the matching renderer command at both sides of the overview boundary.
#[test]
fn agent_cycle_dispatches_overview_and_agent_transitions() {
    let known = Arc::new(Mutex::new(vec!["alpha".to_owned(), "bravo".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "alpha".to_owned(),
        "bravo".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::new()));
    let routing = routing_state(known, live, suspended);
    let (wake_tx, _wake_rx) = tau_blocking_notify_channel::channel();
    let (renderer_tx, renderer_rx) = LocalRendererSender::channel(
        Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
        Arc::new(Mutex::new(())),
        wake_tx,
    );

    routing.set_selected_agent(Some(agent_id("bravo")));
    assert_eq!(
        dispatch_agent_cycle(&routing, &renderer_tx, 1),
        AgentCycleAction::ClearSelection
    );
    assert_eq!(routing.selected_agent_id(), None);
    assert!(matches!(
        renderer_rx.try_recv().expect("overview renderer command"),
        RendererCmd::ClearSelectedAgent
    ));

    assert_eq!(
        dispatch_agent_cycle(&routing, &renderer_tx, 1),
        AgentCycleAction::Select(agent_id("alpha"))
    );
    assert_eq!(routing.selected_agent_id().as_deref(), Some("alpha"));
    assert!(matches!(
        renderer_rx.try_recv().expect("agent renderer command"),
        RendererCmd::SwitchAgent { agent_id } if agent_id.as_str() == "alpha"
    ));

    routing.set_selected_agent(None);
    assert_eq!(
        dispatch_agent_cycle(&routing, &renderer_tx, -1),
        AgentCycleAction::Select(agent_id("bravo"))
    );
    assert!(matches!(
        renderer_rx.try_recv().expect("reverse renderer command"),
        RendererCmd::SwitchAgent { agent_id } if agent_id.as_str() == "bravo"
    ));
}

#[test]
fn agent_completer_uses_display_names_as_descriptions() {
    // `:agent ... <agent_id>` keeps ids as values but shows the durable
    // display name in the completion description so long names remain visible.
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let names = Arc::new(Mutex::new(HashMap::from([(
        agent_id("worker"),
        "Investigate worker".to_owned(),
    )])));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(path_std_collections::HashSet::new()));
    let completer = build_agent_arg_completer(routing_state(known, live, suspended), names);

    let completions = completer(&["switch", "worker"]);

    assert_eq!(completions[0].value, "worker");
    assert_eq!(completions[0].description, "Investigate worker");
}

/// Typing an optional `@` filters the same agent set and completes back to the
/// existing canonical bare-id command text without duplicate or `none` entries.
#[test]
fn agent_completer_accepts_prefixed_needles_and_returns_canonical_ids() {
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(path_std_collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let completer = build_agent_arg_completer(
        routing_state(known, live, Arc::new(Mutex::new(Default::default()))),
        Arc::new(Mutex::new(HashMap::new())),
    );

    let values: Vec<_> = completer(&["switch", "@work"])
        .into_iter()
        .map(|item| item.value)
        .collect();
    assert_eq!(values, vec!["worker"]);

    let values: Vec<_> = completer(&["switch", "@"])
        .into_iter()
        .map(|item| item.value)
        .collect();
    assert_eq!(values, vec!["helper", "worker"]);
}

fn groups() -> Vec<tau_proto::HarnessRoleGroup> {
    vec![
        tau_proto::HarnessRoleGroup {
            name: "engineer".to_owned(),
            roles: vec![
                "engineer-junior".to_owned(),
                "engineer".to_owned(),
                "engineer-senior".to_owned(),
            ],
        },
        tau_proto::HarnessRoleGroup {
            name: "assistant".to_owned(),
            roles: vec!["assistant".to_owned()],
        },
        tau_proto::HarnessRoleGroup {
            name: "manager".to_owned(),
            roles: vec!["manager".to_owned()],
        },
    ]
}

#[test]
fn group_cycle_returns_to_last_runtime_role_for_group() {
    // Tab moves between groups, but returning to a group should restore the
    // role the user last used in that group during this process.
    let groups = groups();
    let mut memory = HashMap::new();
    memory.insert("engineer".to_owned(), "engineer-senior".to_owned());

    assert_eq!(
        next_role_in_groups(Some("manager"), &groups, false, &memory).as_deref(),
        Some("engineer-senior")
    );
}

#[test]
fn group_cycle_ignores_stale_runtime_group_memory() {
    // Role availability can change after startup, so stale remembered roles
    // must not win over the currently configured group contents.
    let groups = groups();
    let mut memory = HashMap::new();
    memory.insert("engineer".to_owned(), "missing-engineer".to_owned());

    assert_eq!(
        next_role_in_groups(Some("manager"), &groups, false, &memory).as_deref(),
        Some("engineer-junior")
    );
}
