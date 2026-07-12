use super::*;
fn routing_state(
    known: Arc<Mutex<Vec<String>>>,
    live: Arc<Mutex<std::collections::HashSet<String>>>,
    suspended: Arc<Mutex<std::collections::HashSet<String>>>,
) -> InputRoutingState {
    let mut navigation = AgentNavigation::default();
    for agent_id in live.lock().expect("live agents lock").iter() {
        navigation.mark_live(agent_id.clone());
    }
    for agent_id in suspended.lock().expect("suspended agents lock").iter() {
        navigation.set_mode(agent_id.clone(), AgentNavigationState::Suspended);
    }
    InputRoutingState::new(
        Arc::new(Mutex::new(None)),
        known,
        Arc::new(Mutex::new(navigation)),
        Arc::new(Mutex::new(std::collections::HashSet::new())),
    )
}

#[test]
fn agent_completer_offers_subcommands_first() {
    // `/agent` is now a command group; the first argument must guide users
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
            ("suspend", "Hide an active agent transcript"),
            ("resume", "Keep a hidden agent in navigation"),
            ("name", "Set an agent display name"),
        ]
    );
}

#[test]
fn session_completer_offers_new_subcommand() {
    // `/session new` is the session-level fresh-start command; `/new` is
    // reserved as an alias for `/agent new`.
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
    // `/agent new` only clears the selected agent; it must not offer or
    // accept an agent-id argument like switch/suspend/resume do.
    let completer = build_agent_arg_completer(
        routing_state(
            Arc::new(Mutex::new(vec!["worker".to_owned()])),
            Arc::new(Mutex::new(std::collections::HashSet::from([
                "worker".to_owned()
            ]))),
            Arc::new(Mutex::new(Default::default())),
        ),
        Arc::new(Mutex::new(HashMap::new())),
    );

    assert!(completer(&["new", ""]).is_empty());
}

#[test]
fn agent_suspend_resume_updates_prompt_routing_state_synchronously() {
    // Regression: `/agent suspend` and `/agent resume` are initiated by the
    // input thread, while the renderer applies the UI command later. Mirror
    // the state immediately so a prompt entered on the next line observes
    // the updated active/suspended sets without racing the renderer thread.
    let routing = routing_state(
        Arc::new(Mutex::new(vec!["worker".to_owned()])),
        Arc::new(Mutex::new(std::collections::HashSet::from([
            "worker".to_owned()
        ]))),
        Arc::new(Mutex::new(std::collections::HashSet::new())),
    );
    routing.mark_suspended("worker");
    assert!(!routing.agent_is_active("worker"));
    routing.mark_resumed("worker");
    assert!(routing.agent_is_active("worker"));
}

#[test]
fn selected_agent_suspend_alias_dispatches_existing_suspend_flow() {
    // `/suspend` is a no-argument alias for suspending the selected agent. This
    // verifies the command path updates prompt-routing state immediately and
    // emits the renderer command used by `/agent suspend`.
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let routing = routing_state(known, live.clone(), suspended.clone());
    routing.set_selected_agent(Some("worker".to_owned()));
    let (renderer_tx, renderer_rx) = mpsc::channel();
    let messages = Arc::new(Mutex::new(Vec::new()));

    handle_agent_suspend_command(&routing, &renderer_tx, None, &|message| {
        messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.to_owned());
    });

    assert!(messages.lock().expect("messages lock poisoned").is_empty());
    assert!(!routing.agent_is_active("worker"));
    match renderer_rx.try_recv().expect("renderer command") {
        RendererCmd::SuspendAgent { agent_id } => assert_eq!(agent_id, "worker"),
        _ => panic!("expected suspend renderer command"),
    }
}

#[test]
fn selected_agent_resume_alias_dispatches_existing_resume_flow() {
    // `/resume` is a no-argument alias for resuming the selected suspended
    // agent. This catches regressions where the alias updates state but forgets
    // to notify the renderer, or vice versa.
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::from([
        "worker".to_owned()
    ])));
    let routing = routing_state(known, live.clone(), suspended.clone());
    routing.set_selected_agent(Some("worker".to_owned()));
    let (renderer_tx, renderer_rx) = mpsc::channel();
    let messages = Arc::new(Mutex::new(Vec::new()));

    handle_agent_resume_command(&routing, &renderer_tx, None, &|message| {
        messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.to_owned());
    });

    assert!(messages.lock().expect("messages lock poisoned").is_empty());
    assert!(routing.agent_is_active("worker"));
    match renderer_rx.try_recv().expect("renderer command") {
        RendererCmd::ResumeAgent { agent_id } => assert_eq!(agent_id, "worker"),
        _ => panic!("expected resume renderer command"),
    }
}

#[test]
fn agent_mention_completer_offers_only_active_agents() {
    // Prompt-text `@agent` completion is for routing to active agents. It
    // must not suggest suspended agents even though `/agent resume` does.
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::from([
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
    navigation.mark_active_auto_if_absent("helper");
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

    navigation
        .lock()
        .expect("agent navigation")
        .update_runtime("helper", tau_proto::AgentRuntimeState::Running);
    assert_eq!(mentions(&[""])[0].value, "helper");
    assert_eq!(agents(&["switch", ""])[1].value, "helper");
    assert_eq!(agents(&["suspend", ""])[0].value, "helper");
    assert!(agents(&["resume", ""]).is_empty());

    navigation
        .lock()
        .expect("agent navigation")
        .update_runtime("helper", tau_proto::AgentRuntimeState::Idle);
    assert!(mentions(&[""]).is_empty());
    assert_eq!(agents(&["resume", ""])[0].value, "helper");
}

#[test]
fn agent_completer_filters_active_and_suspended_agents() {
    // Suspended delegate agents should disappear from switch/suspend menus so
    // tab completion stays focused on active choices, but remain available for
    // explicit resume.
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::from([
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

#[test]
fn agent_switch_accepts_explicit_suspended_agent_id() {
    // Explicit `/agent switch <agent_id>` is intentional routing by id. This
    // command path must select a known suspended agent and notify the renderer
    // without printing the resume-only local error that completions are meant to
    // avoid.
    let known = Arc::new(Mutex::new(vec!["helper".to_owned(), "worker".to_owned()]));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "helper".to_owned(),
        "worker".to_owned(),
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::from([
        "helper".to_owned()
    ])));
    let routing = routing_state(known, live, suspended);
    let (renderer_tx, renderer_rx) = mpsc::channel();
    let messages = Arc::new(Mutex::new(Vec::new()));

    handle_agent_switch_command(&routing, &renderer_tx, Some("helper"), &|message| {
        messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.to_owned());
    });

    assert!(messages.lock().expect("messages lock poisoned").is_empty());
    assert_eq!(routing.selected_agent_id().as_deref(), Some("helper"));
    match renderer_rx.try_recv().expect("renderer command") {
        RendererCmd::SwitchAgent { agent_id } => assert_eq!(agent_id, "helper"),
        _ => panic!("expected switch renderer command"),
    }
}

#[test]
fn agent_completer_uses_display_names_as_descriptions() {
    // `/agent ... <agent_id>` keeps ids as values but shows the durable
    // display name in the completion description so long names remain visible.
    let known = Arc::new(Mutex::new(vec!["worker".to_owned()]));
    let names = Arc::new(Mutex::new(HashMap::from([(
        "worker".to_owned(),
        "Investigate worker".to_owned(),
    )])));
    let live = Arc::new(Mutex::new(std::collections::HashSet::from([
        "worker".to_owned()
    ])));
    let suspended = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let completer = build_agent_arg_completer(routing_state(known, live, suspended), names);

    let completions = completer(&["switch", "worker"]);

    assert_eq!(completions[0].value, "worker");
    assert_eq!(completions[0].description, "Investigate worker");
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
