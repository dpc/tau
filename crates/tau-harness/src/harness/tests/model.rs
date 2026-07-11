use tau_proto::{
    Effort, ModelId, NoticeLevel, ProviderModelInfo, ProviderModelsUpdated, ThinkingSummary,
    Verbosity,
};

use super::*;
use crate::model::LoadedRoles;

/// Scan the harness event log for a mandatory warning `HarnessNotice`
/// containing `needle` and return its message. The startup paths emit
/// these synchronously before the constructor returns, so by the time
/// the test inspects the log every check_*_parses event is already
/// committed — no need to pump the bus.
fn find_mandatory_warning_notice(h: &Harness, needle: &str) -> Option<String> {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::HarnessNotice(info) = &entry.event
            && info.level == NoticeLevel::Warning
            && info.message.contains(needle)
        {
            return Some(info.message.clone());
        }
    }
    None
}

fn find_mandatory_config_error_notice(h: &Harness, needle: &str) -> Option<String> {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::HarnessNotice(info) = &entry.event
            && info.level == NoticeLevel::Warning
            && info.always_show
            && info.kind == tau_proto::notice_kind::HARNESS_CONFIG_ERROR
            && info.message.contains(needle)
        {
            return Some(info.message.clone());
        }
    }
    None
}

fn find_info(h: &Harness, needle: &str) -> Option<String> {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::HarnessNotice(info) = &entry.event
            && info.message.contains(needle)
        {
            return Some(info.message.clone());
        }
    }
    None
}

fn provider_model(id: ModelId, context_window: u64) -> ProviderModelInfo {
    ProviderModelInfo {
        id,
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: vec![],
        default_affinity: 0,
        context_window,
        efforts: vec![Effort::High],
        verbosities: vec![Verbosity::Low, Verbosity::High],
        thinking_summaries: vec![ThinkingSummary::Off, ThinkingSummary::Auto],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
    }
}

fn provider_models(
    models: impl IntoIterator<Item = ProviderModelInfo>,
) -> std::collections::HashMap<ModelId, ProviderModelInfo> {
    models
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect()
}

/// The echo harness publishes `echo/model` during startup so daemon-style tests
/// exercise the normal provider model route. Tests that assert the
/// before-any-model-snapshot state clear that startup snapshot first.
fn clear_startup_echo_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("echo provider")
        .to_owned();
    h.handle_extension_event(
        &provider_id,
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: Vec::new(),
        })),
    )
    .expect("clear startup echo provider models");
}

fn connect_provider_source(h: &mut Harness, name: &str) {
    let _frames = connect_test_client(h, name, tau_proto::ClientKind::Provider);
}

/// Role info keeps the machine-readable model/knob summary separate from the
/// free-form role description so completion UIs do not have to parse user text.
#[test]
fn role_infos_include_configured_role_description() {
    let model: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let mut roles = std::collections::HashMap::new();
    roles.insert(
        "engineer".to_owned(),
        tau_config::settings::AgentRole {
            description: Some("Balanced coding helper".to_owned()),
            model: Some(model.clone()),
            effort: Some(Effort::High),
            ..Default::default()
        },
    );
    let provider_models = provider_models([provider_model(model.clone(), 128_000)]);
    let infos = role_infos(&provider_models, &roles, std::slice::from_ref(&model));

    assert_eq!(infos.len(), 1);
    assert!(infos[0].description.contains("model=openai/gpt-4.1"));
    assert_eq!(
        infos[0].role_description.as_deref(),
        Some("Balanced coding helper")
    );
    let details = infos[0].details.as_ref().expect("structured role details");
    assert_eq!(details.model.as_ref(), Some(&model));
    assert_eq!(details.params.effort, Effort::High);
}

/// Ensures role group navigation honors explicit role `order` values before
/// falling back to role names, so UI cycling can use logical sequences such as
/// engineer-junior -> engineer -> engineer-senior even when config or map
/// iteration is different.
#[test]
fn role_groups_sort_roles_by_order_then_name() {
    let roles = std::collections::HashMap::from([
        (
            "engineer-senior".to_owned(),
            tau_config::settings::AgentRole {
                order: Some(30),
                ..Default::default()
            },
        ),
        (
            "engineer-junior".to_owned(),
            tau_config::settings::AgentRole {
                order: Some(10),
                ..Default::default()
            },
        ),
        (
            "engineer".to_owned(),
            tau_config::settings::AgentRole {
                order: Some(20),
                ..Default::default()
            },
        ),
        (
            "alpha-peer".to_owned(),
            tau_config::settings::AgentRole {
                order: Some(20),
                ..Default::default()
            },
        ),
        (
            "omega-explicit-max".to_owned(),
            tau_config::settings::AgentRole {
                order: Some(i64::MAX),
                ..Default::default()
            },
        ),
        (
            "alpha-unordered".to_owned(),
            tau_config::settings::AgentRole::default(),
        ),
        (
            "advisor".to_owned(),
            tau_config::settings::AgentRole::default(),
        ),
    ]);
    let configured_groups = [tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec![
            "alpha-unordered".to_owned(),
            "omega-explicit-max".to_owned(),
            "engineer-senior".to_owned(),
            "engineer".to_owned(),
            "advisor".to_owned(),
            "alpha-peer".to_owned(),
            "engineer-junior".to_owned(),
        ],
    }];

    let groups = crate::model::role_groups_for_roles(&roles, &configured_groups);

    assert_eq!(
        groups[0].roles,
        vec![
            "engineer-junior",
            "alpha-peer",
            "engineer",
            "engineer-senior",
            "omega-explicit-max",
            "advisor",
            "alpha-unordered",
        ]
    );
}

/// Provider snapshots are runtime registry input, not just private extension
/// chatter: the harness must retain metadata/routes and re-emit refreshed UI
/// state for clients that are already connected.
#[test]
fn provider_models_snapshot_updates_available_models() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    connect_provider_source(&mut h, "provider-ext");

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    assert!(!h.available_models.contains(&model_id));
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");
    assert!(h.available_models.contains(&model_id));
    assert_eq!(
        h.provider_model_info
            .get(&model_id)
            .map(|info| info.context_window),
        Some(128_000),
    );
    assert_eq!(
        h.provider_model_routes.get(&model_id).map(|id| id.as_str()),
        Some("provider-ext"),
    );

    let mut saw_provider_snapshot = false;
    let mut saw_harness_models = false;
    let mut saw_harness_roles = false;
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match entry.event {
            Event::ProviderModelsUpdated(update)
                if entry.source.as_deref() == Some("provider-ext") =>
            {
                saw_provider_snapshot = update.models.iter().any(|info| info.id == model_id);
            }
            Event::HarnessModelsAvailable(available) => {
                saw_harness_models = available.models.contains(&model_id);
            }
            Event::HarnessRolesAvailable(_) => {
                saw_harness_roles = true;
            }
            _ => {}
        }
    }
    assert!(saw_provider_snapshot);
    assert!(saw_harness_models);
    assert!(saw_harness_roles);
}

/// Model snapshots are an execution-provider contract. A tool connection that
/// publishes `provider.models_updated` must not be able to claim a model route,
/// otherwise the next prompt could be sent to a non-provider participant.
#[test]
fn provider_models_snapshot_from_non_provider_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    let _frames = connect_test_client(&mut h, "tool-ext", tau_proto::ClientKind::Tool);

    let model_id: ModelId = "evil/model".parse().expect("model id");
    h.handle_extension_event(
        "tool-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 1)],
        })),
    )
    .expect("handle forged provider snapshot");

    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_info.contains_key(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        assert!(
            !matches!(entry.event, Event::ProviderModelsUpdated(_))
                || entry.source.as_deref() != Some("tool-ext"),
            "forged provider snapshot must not be published"
        );
    }
}

/// Socket clients are UI participants. Even though their frames enter through
/// the client handler instead of the extension handler, provider-category
/// events from them must not mutate provider routing or get published.
#[test]
fn provider_models_snapshot_from_ui_client_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    let _frames = connect_test_client(&mut h, "ui-client", tau_proto::ClientKind::Ui);

    let model_id: ModelId = "evil/ui-model".parse().expect("model id");
    h.handle_client_event_inner(
        "ui-client",
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 1)],
        }),
    )
    .expect("handle forged client provider snapshot");

    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_info.contains_key(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        assert!(
            !matches!(entry.event, Event::ProviderModelsUpdated(_))
                || entry.source.as_deref() != Some("ui-client"),
            "client-forged provider snapshot must not be published"
        );
    }
}

/// Roles without an explicit model should use provider intent, not incidental
/// lexicographic model ordering. This lets providers steer Tau's implicit
/// default while keeping role model overrides exact.
#[test]
fn role_without_model_selects_highest_default_affinity() {
    let low: ModelId = "openai/aaa-cheap".parse().expect("model id");
    let high: ModelId = "openai/zzz-engineer".parse().expect("model id");
    let mut low_info = provider_model(low.clone(), 128_000);
    low_info.default_affinity = 10;
    let mut high_info = provider_model(high.clone(), 128_000);
    high_info.default_affinity = 100;
    let provider_models = provider_models([low_info, high_info]);
    let roles = std::collections::HashMap::from([(
        "engineer".to_owned(),
        tau_config::settings::AgentRole::default(),
    )]);

    assert_eq!(
        select_model_for_role(&provider_models, &roles, "engineer"),
        Some(high)
    );
}

/// Startup no longer selects config-file models. A provider snapshot is the
/// moment a runtime model exists, so it should also unblock queued prompts by
/// choosing the default-affinity model through the normal harness-owned
/// selection path.
#[test]
fn provider_models_snapshot_selects_first_model_and_drains_queue() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    assert!(h.selected_model.is_none());

    assert_eq!(
        h.submit_user_prompt("s1".into(), "hello".to_owned())
            .expect("submit prompt"),
        PromptSubmission::Queued,
    );
    assert_eq!(h.agents[&test_user_agent(&h)].pending_prompts.len(), 1,);

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    assert_eq!(h.selected_model.as_ref(), Some(&model_id));
    assert_eq!(h.selected_model_params().effort, Effort::High);
    let conv = &h.agents[&test_user_agent(&h)];
    assert!(conv.pending_prompts.is_empty());
    assert!(matches!(
        conv.turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
}

/// `/model <provider>/<model>` is an agent-local selection, not a role switch
/// or role mutation. Future prompts for that loaded agent should resolve to the
/// override while the agent's role stays unchanged.
#[test]
fn ui_agent_model_select_sets_model_override_for_target_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    let role = h.selected_role.clone();
    let cid = h.create_durable_user_agent("s1".into(), &role);
    let agent_id = h.agents[&cid].agent_id.clone().expect("durable agent id");

    let default_model: ModelId = "test/default".parse().expect("model id");
    let selected_model: ModelId = "test/selected".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![
                provider_model(default_model, 128_000),
                provider_model(selected_model.clone(), 128_000),
            ],
        })),
    )
    .expect("handle provider snapshot");

    h.handle_client_event_inner(
        "ui-client",
        Event::UiAgentModelSelect(tau_proto::UiAgentModelSelect {
            session_id: "s1".into(),
            target_agent_id: Some(crate::parse_agent_id(&agent_id)),
            model: selected_model.clone(),
        }),
    )
    .expect("handle model select");

    let conv = &h.agents[&cid];
    assert_eq!(conv.role.as_deref(), Some(role.as_str()));
    assert_eq!(conv.model_override.as_ref(), Some(&selected_model));
    assert_eq!(h.model_for_agent_role(conv), Some(selected_model.clone()));

    h.agents.get_mut(&cid).expect("agent").role = None;
    assert_eq!(
        h.model_for_agent_role(&h.agents[&cid]),
        Some(selected_model)
    );
}

/// Creating an agent may include the model override staged by the interactive
/// `/new` + `/model` flow; the harness must apply it before the first prompt is
/// routed so the initial provider request uses the requested model.
#[test]
fn ui_create_agent_applies_initial_model_override() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    let role = h.selected_role.clone();
    let default_model: ModelId = "test/default".parse().expect("model id");
    let selected_model: ModelId = "test/selected".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![
                provider_model(default_model.clone(), 128_000),
                provider_model(selected_model.clone(), 128_000),
            ],
        })),
    )
    .expect("handle provider snapshot");

    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        parent_agent: None,
        session_id: "s1".into(),
        role,
        model_override: Some(selected_model.clone()),
        metadata: Vec::new(),
        initial_prompt: Some("hello".to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    })
    .expect("create agent");

    let cid = test_user_agent(&h);
    let conv = &h.agents[&cid];
    assert_eq!(conv.model_override.as_ref(), Some(&selected_model));
    assert_eq!(h.model_for_agent_role(conv), Some(selected_model));
    let created = read_nth_prompt_created(&h, 0);
    assert_eq!(created.model, "test/selected".parse().expect("model id"));
}

/// A model staged by `/new` + `/model` must survive the supported cold-provider
/// path where the first prompt queues before any provider has published models.
#[test]
fn ui_create_agent_preserves_model_override_until_cold_provider_models_arrive() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    h.provider_model_routes.clear();
    h.provider_model_info.clear();
    h.available_models.clear();
    h.selected_model = None;
    let role = h.selected_role.clone();
    let selected_model: ModelId = "test/cold-selected".parse().expect("model id");

    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        parent_agent: None,
        session_id: "s1".into(),
        role,
        model_override: Some(selected_model.clone()),
        metadata: Vec::new(),
        initial_prompt: Some("hello cold".to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    })
    .expect("create queued agent");

    let cid = test_user_agent(&h);
    assert_eq!(
        h.agents[&cid].model_override.as_ref(),
        Some(&selected_model)
    );
    assert!(
        event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptQueued(_)))
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );

    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(selected_model.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    let created = read_nth_prompt_created(&h, 0);
    assert_eq!(created.model, selected_model);
}

/// If a model override disappears from provider routing, the harness must not
/// emit future prompts to that unrouteable model. It should fall back to normal
/// role-based resolution instead.
#[test]
fn unavailable_agent_model_override_falls_back_to_role_model() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    let role = h.selected_role.clone();
    let cid = h.create_durable_user_agent("s1".into(), &role);
    let role_model: ModelId = "test/role-model".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(role_model.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    h.agents.get_mut(&cid).expect("agent").model_override =
        Some("test/missing".parse().expect("model id"));

    assert_eq!(h.model_for_agent_role(&h.agents[&cid]), Some(role_model));
}

/// A target-less `/model` request is only safe when the session has exactly one
/// loaded user agent. With multiple user agents the UI must send an explicit
/// target so the harness does not depend on `HashMap` iteration order.
#[test]
fn targetless_agent_model_select_rejects_ambiguous_user_agents() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    let role = h.selected_role.clone();
    let first = h.create_durable_user_agent("s1".into(), &role);
    let second = h.create_durable_user_agent("s1".into(), &role);
    let selected_model: ModelId = "test/selected".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(selected_model.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    h.handle_client_event_inner(
        "ui-client",
        Event::UiAgentModelSelect(tau_proto::UiAgentModelSelect {
            session_id: "s1".into(),
            target_agent_id: None,
            model: selected_model,
        }),
    )
    .expect("handle model select");

    assert!(h.agents[&first].model_override.is_none());
    assert!(h.agents[&second].model_override.is_none());
}

/// A role with an explicit model that no provider advertised is a terminal
/// configuration problem after provider metadata exists. The harness must not
/// leave the first prompt in `pending_prompts` forever just because the
/// selected model is `None`; it should attempt dispatch, surface the existing
/// no-model diagnostic, and return the agent to idle.
#[test]
fn unavailable_explicit_role_model_does_not_stall_queued_prompt() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    assert!(h.provider_model_info.is_empty());
    assert!(h.selected_model.is_none());

    let missing_model: ModelId = "missing/provider-model".parse().expect("model id");
    h.available_roles.insert(
        "assistant".to_owned(),
        tau_config::settings::AgentRole {
            model: Some(missing_model),
            ..Default::default()
        },
    );
    h.selected_role = "assistant".to_owned();

    assert_eq!(
        h.submit_user_prompt("s1".into(), "hello".to_owned())
            .expect("submit prompt"),
        PromptSubmission::Queued,
    );
    assert_eq!(h.agents[&test_user_agent(&h)].pending_prompts.len(), 1);

    let available_model: ModelId = "openai/available".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(available_model, 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    let conv = &h.agents[&test_user_agent(&h)];
    assert!(conv.pending_prompts.is_empty());
    assert!(matches!(conv.turn_state, AgentTurnState::Idle));
    assert!(
        find_info(&h, "role `assistant` has no available model").is_some(),
        "missing model should be visible instead of silently wedging"
    );
}

/// Provider metadata must replace config compat data once a provider-owned
/// model is selected, otherwise the UI loses context-window and knob choices.
#[test]
fn provider_model_metadata_drives_selection_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 123_456)],
        })),
    )
    .expect("handle provider snapshot");

    assert_eq!(h.selected_role, "engineer");
    assert_eq!(h.selected_model.as_ref(), Some(&model_id));
    assert_eq!(h.selected_model_params().effort, Effort::High);
    assert_eq!(h.selected_model_params().verbosity, Verbosity::Low);
    assert_eq!(
        h.selected_model_params().thinking_summary,
        ThinkingSummary::Auto
    );

    let mut seq = crate::event_log::EventLogSeq::new(0);
    let mut selected = None;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::HarnessRoleSelected(event) = entry.event
            && event.model.as_ref() == Some(&model_id)
        {
            selected = Some(event);
        }
    }
    let selected = selected.expect("model selection event");
    assert_eq!(selected.context_window, Some(123_456));

    h.handle_extension_event(
        "provider-ext",
        TestProtocolItem::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: model_id.clone(),
                display_name: None,
                tags: Vec::new(),
                supported_tool_types: vec![],
                default_affinity: 0,
                context_window: 654_321,
                efforts: vec![Effort::Off],
                verbosities: vec![Verbosity::High],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: false,
                standalone_compaction_threshold: None,
            }],
        })),
    )
    .expect("refresh provider metadata");

    assert_eq!(h.selected_model_params().effort, Effort::Off);
    assert_eq!(h.selected_model_params().verbosity, Verbosity::High);
    assert_eq!(
        h.selected_model_params().thinking_summary,
        ThinkingSummary::Off
    );

    let mut seq = crate::event_log::EventLogSeq::new(0);
    let mut selected = None;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::HarnessRoleSelected(event) = entry.event
            && event.model.as_ref() == Some(&model_id)
        {
            selected = Some(event);
        }
    }
    let selected = selected.expect("refreshed model selection event");
    assert_eq!(selected.context_window, Some(654_321));
}

/// Selected role params come from the role, then clamp against provider-owned
/// metadata for the resolved model. This keeps runtime selection role-centric
/// while still respecting each provider's supported knob levels.
#[test]
fn selected_role_params_are_clamped_by_provider_metadata() {
    let openai: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![Effort::Off, Effort::High],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);

    let mut roles = std::collections::HashMap::new();
    let mut openai_role = tau_config::settings::AgentRole {
        model: Some(openai.clone()),
        effort: Some(Effort::High),
        ..Default::default()
    };
    roles.insert("openai".to_owned(), openai_role.clone());
    openai_role.model = Some(local.clone());
    roles.insert("local".to_owned(), openai_role);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "openai", &openai).effort,
        Effort::High,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "local", &local).effort,
        Effort::Off,
    );
}

/// Stale harness state is ignored now that role edits are runtime-only.
/// Startup should use `harness.yaml` as the only role source.
#[test]
fn load_roles_ignores_stale_harness_state() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                default_role: "engineer",
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { model: "openai/gpt-4.1", effort: "high", verbosity: "medium" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json"),
        r#"{
            "role_overrides": {
                "engineer": { "model": "openai/gpt-4.1-mini", "effort": "low", "verbosity": "high" }
            }
        }"#,
    )
    .expect("write stale state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let LoadedRoles {
        roles,
        role_overrides,
        selected_role,
        role_groups: _role_groups,
        missing_default_role: _missing_default_role,
    } = load_roles(&harness_settings);
    assert!(role_overrides.is_empty());
    assert_eq!(selected_role, "engineer");
    let role = roles.get("engineer").expect("engineer role");
    assert_eq!(
        role.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-4.1")
    );
    assert_eq!(role.effort, Some(Effort::High));
    assert_eq!(role.verbosity, Some(Verbosity::Medium));
}

/// Roles without an explicit effort get the middle provider-published
/// reasoning level. Providers that publish only `Off` stay at `Off`.
#[test]
fn role_without_effort_picks_middle_provider_effort() {
    let openai: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![
                Effort::Off,
                Effort::Minimal,
                Effort::Low,
                Effort::Medium,
                Effort::High,
            ],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);
    let roles = std::collections::HashMap::from([(
        "engineer".to_owned(),
        tau_config::settings::AgentRole::default(),
    )]);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "engineer", &openai).effort,
        Effort::Low,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "engineer", &local).effort,
        Effort::Off,
    );
}

/// A stale saved `default` role is not migrated. Runtime models are
/// provider-owned, so startup keeps the `engineer` role and waits for a
/// provider snapshot before selecting a model.
#[test]
fn load_roles_falls_back_to_engineer_role_while_models_are_provider_owned() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                default_role: "engineer",
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { model: "local/engineer" },
                    },
                },
                manager: {
                    roles: {
                        manager: { model: "local/deep" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json"),
        r#"{
            "role_overrides": {
                "default": { "model": "local/deep" }
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let LoadedRoles {
        roles,
        role_overrides,
        selected_role,
        role_groups: _role_groups,
        missing_default_role: _missing_default_role,
    } = load_roles(&harness_settings);
    assert!(!role_overrides.contains_key("default"));
    assert!(!roles.contains_key("default"));
    assert_eq!(selected_role, "engineer");

    let available = ["local/deep".into(), "local/engineer".into()];
    let provider_models = provider_models(
        available
            .iter()
            .cloned()
            .map(|model| provider_model(model, 8_192)),
    );
    assert_eq!(
        select_model_for_role(&provider_models, &roles, &selected_role)
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("local/engineer")
    );
}

/// Role settings stand on their own: a non-engineer role with no model or
/// effort uses the first available model and the selected model's default
/// effort, not engineer's configured model or effort.
#[test]
fn role_missing_fields_use_model_defaults() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                default_role: "plain",
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { model: "local/engineer", effort: "high" },
                        plain: {},
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness config");
    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let LoadedRoles {
        roles,
        role_overrides: _role_overrides,
        selected_role,
        role_groups: _role_groups,
        missing_default_role: _missing_default_role,
    } = load_roles(&harness_settings);
    let available = ["local/aaa".into(), "local/engineer".into()];
    let available_provider_models = provider_models(
        available
            .iter()
            .cloned()
            .map(|model| provider_model(model, 8_192)),
    );
    let selected = select_model_for_role(&available_provider_models, &roles, &selected_role)
        .expect("selected model");
    assert_eq!(selected_role, "plain");
    assert_eq!(selected.to_string(), "local/aaa");

    let provider_models = provider_models([ProviderModelInfo {
        id: selected.clone(),
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: vec![],
        default_affinity: 0,
        context_window: 8_192,
        efforts: vec![Effort::Off, Effort::Low, Effort::High],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
    }]);
    let params = selected_params_for_role(&provider_models, &roles, "plain", &selected);
    assert_eq!(params.effort, Effort::Low);
}

/// Roles without an explicit verbosity default to low when the provider
/// supports the knob, keeping replies concise unless the user opts into more
/// detail. Providers without verbosity support publish a single fixed level.
#[test]
fn role_without_verbosity_picks_low_when_supported() {
    let openai: ModelId = "openai/gpt-5".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);
    let roles = std::collections::HashMap::from([(
        "engineer".to_owned(),
        tau_config::settings::AgentRole::default(),
    )]);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "engineer", &openai).verbosity,
        Verbosity::Low,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "engineer", &local).verbosity,
        Verbosity::Medium,
    );
}

/// A malformed `harness.yaml` must surface in the UI as a mandatory warning
/// `HarnessNotice`. Without this, the only symptom of a borked file is that
/// user-configured extensions or roles vanish.
#[test]
fn borked_harness_yaml_emits_mandatory_warning_notice() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        "{ extensions: { foo: { command: [ \"echo\" ",
    )
    .expect("write borked harness");

    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("harness");
    let message = find_mandatory_warning_notice(&h, "harness.yaml")
        .expect("expected mandatory warning HarnessNotice about harness.yaml");
    assert!(
        message.contains("failed to parse"),
        "message should explain what happened, got: {message}"
    );
}

/// Disabling every role is a configuration error: startup must fail clearly
/// instead of selecting the built-in fallback role name even though it was
/// filtered out.
#[test]
fn harness_startup_errors_when_no_roles_are_enabled() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    roles: {
                        "engineer": { enable: false },
                        "engineer-junior": { enable: false },
                        "engineer-senior": { enable: false },
                    },
                },
                manager: {
                    roles: {
                        "micro-manager": { enable: false },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness config");

    let error = match echo_harness_with_dirs("s1", state_dir, dirs) {
        Ok(_) => panic!("startup should fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("no roles are enabled"),
        "error should explain that every role was disabled, got: {error}"
    );
}

/// Ensures a non-selected role with a missing required skill is removed from
/// selectable/delegatable role state and surfaced as a mandatory configuration
/// notice instead of letting an agent best-effort without its role skill.
#[test]
fn missing_required_skill_disables_role_and_emits_notice() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  required_skills: [missing-review-skill]
        "#,
    )
    .expect("write harness config");

    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("harness");

    assert!(!h.available_roles.contains_key("reviewer"));
    assert!(
        h.disabled_role_reasons.contains_key("reviewer"),
        "disabled role reason should be retained for later UI/delegation errors"
    );
    let message = find_mandatory_warning_notice(&h, "role `reviewer` disabled")
        .expect("expected required-skill warning notice");
    assert!(message.contains("`missing-review-skill` is not discovered"));
    assert!(message.contains("required_skills"));
}

/// Ensures each documented required-skill disable reason is reported through a
/// mandatory `harness.config_error` notice. These diagnostics are the user's
/// actionable explanation for why a configured role disappeared.
#[test]
fn required_skill_disable_reasons_are_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.available_roles.insert(
        "invalid-required-skill".to_owned(),
        tau_config::settings::AgentRole {
            required_skills: vec![tau_proto::SkillName::from("Bad_Name")],
            ..Default::default()
        },
    );

    let hidden_path = td.path().join("hidden-skill.md");
    std::fs::write(
        &hidden_path,
        "---\nname: hidden-required-skill\ndescription: Hidden\n---\nbody\n",
    )
    .expect("write hidden skill");
    h.record_discovered_skill(
        "test-skills",
        &tau_proto::ExtSkillAvailable {
            name: "hidden-required-skill".into(),
            description: "Hidden".to_owned(),
            file_path: hidden_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: true,
            argument_hint: None,
        },
    );
    h.available_roles.insert(
        "hidden-required-skill".to_owned(),
        tau_config::settings::AgentRole {
            required_skills: vec![tau_proto::SkillName::from("hidden-required-skill")],
            ..Default::default()
        },
    );

    let unreadable_path = td.path().join("unreadable-required-skill.md");
    std::fs::write(
        &unreadable_path,
        "---\nname: unreadable-required-skill\ndescription: Unreadable\n---\nbody\n",
    )
    .expect("write unreadable skill");
    h.record_discovered_skill(
        "test-skills",
        &tau_proto::ExtSkillAvailable {
            name: "unreadable-required-skill".into(),
            description: "Unreadable".to_owned(),
            file_path: unreadable_path.clone(),
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );
    std::fs::remove_file(&unreadable_path).expect("remove unreadable skill source");
    h.available_roles.insert(
        "unreadable-required-skill".to_owned(),
        tau_config::settings::AgentRole {
            required_skills: vec![tau_proto::SkillName::from("unreadable-required-skill")],
            ..Default::default()
        },
    );

    h.enforce_required_role_skills().expect("validation");

    assert!(!h.available_roles.contains_key("invalid-required-skill"));
    let invalid = find_mandatory_config_error_notice(&h, "role `invalid-required-skill` disabled")
        .expect("invalid skill diagnostic");
    assert!(invalid.contains("`Bad_Name` has invalid skill name"));
    assert!(invalid.contains("invalid characters"));

    assert!(!h.available_roles.contains_key("hidden-required-skill"));
    let hidden = find_mandatory_config_error_notice(&h, "role `hidden-required-skill` disabled")
        .expect("hidden skill diagnostic");
    assert!(hidden.contains("hidden from model-side skill loading"));

    assert!(!h.available_roles.contains_key("unreadable-required-skill"));
    let unreadable =
        find_mandatory_config_error_notice(&h, "role `unreadable-required-skill` disabled")
            .expect("unreadable skill diagnostic");
    assert!(unreadable.contains("could not be loaded"));
    assert!(unreadable.contains("No such file") || unreadable.contains("os error"));
}

/// Ensures an explicitly selected startup role cannot silently fall back when a
/// required skill is missing. This protects specialized roles such as reviewers
/// from continuing without their mandatory review-process skill.
#[test]
fn selected_role_missing_required_skill_fails_startup() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
        agents:
          default_role: reviewer
          role_groups:
            custom:
              roles:
                reviewer:
                  requiredSkills: [missing-review-skill]
        "#,
    )
    .expect("write harness config");

    let error = match echo_harness_with_dirs("s1", state_dir, dirs) {
        Ok(_) => panic!("selected role with missing skill should fail startup"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("role `reviewer` disabled")
            && error
                .to_string()
                .contains("selected/default role is unavailable"),
        "error should explain the disabled selected role, got: {error}"
    );
}

/// Ensures exact-name required skills accept model-loadable built-in skills so
/// startup validation does not depend on project skill discovery when the
/// requirement is already available in the harness.
#[test]
fn available_required_skill_keeps_role_enabled() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  required_skills: [tau-self-knowledge-config]
        "#,
    )
    .expect("write harness config");

    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("harness");

    assert!(h.available_roles.contains_key("reviewer"));
    assert!(!h.disabled_role_reasons.contains_key("reviewer"));
}

fn delayed_skill_runner(
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
) -> Result<(), String> {
    let mut reader = tau_proto::PeerInputReader::new(std::io::BufReader::new(reader));
    let mut writer = tau_proto::PeerOutputWriter::new(std::io::BufWriter::new(writer));
    writer
        .write_message(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "delayed-skill".into(),
            client_kind: tau_proto::ClientKind::Tool,
        }))
        .map_err(|error| error.to_string())?;
    writer
        .write_message(&tau_proto::HarnessInputMessage::Subscribe(
            tau_proto::Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![tau_proto::EventSelector::Exact(
                    tau_proto::EventName::SESSION_STARTED,
                )],
            },
        ))
        .map_err(|error| error.to_string())?;
    writer
        .write_message(&tau_proto::HarnessInputMessage::emit(
            Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
        ))
        .map_err(|error| error.to_string())?;
    writer
        .write_message(&tau_proto::HarnessInputMessage::emit(
            Event::ExtensionSessionContextProviderRegister(
                tau_proto::ExtensionSessionContextProviderRegister {},
            ),
        ))
        .map_err(|error| error.to_string())?;
    writer
        .write_message(&tau_proto::HarnessInputMessage::Ready(tau_proto::Ready {
            message: None,
        }))
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())?;

    while let Some(message) = reader.read_message().map_err(|error| error.to_string())? {
        let tau_proto::HarnessOutputMessage::Deliver(delivery) = message else {
            continue;
        };
        let Event::SessionStarted(started) = *delivery.event else {
            continue;
        };
        let skill_path = std::env::temp_dir().join(format!(
            "tau-delayed-required-skill-{}-{}.md",
            std::process::id(),
            started.session_id.as_str()
        ));
        std::fs::write(
            &skill_path,
            "---\nname: delayed-required-skill\ndescription: Delayed required skill\n---\nbody\n",
        )
        .map_err(|error| error.to_string())?;
        writer
            .write_message(&tau_proto::HarnessInputMessage::emit(
                Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
                    name: "delayed-required-skill".into(),
                    description: "Delayed required skill".to_owned(),
                    file_path: skill_path,
                    add_to_prompt: false,
                    user_invocable: true,
                    disable_model_invocation: false,
                    argument_hint: None,
                }),
            ))
            .map_err(|error| error.to_string())?;
        writer
            .write_message(&tau_proto::HarnessInputMessage::emit(
                Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
                    session_id: started.session_id,
                }),
            ))
            .map_err(|error| error.to_string())?;
        writer.flush().map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Ensures startup waits for registered context providers to acknowledge
/// session-wide skill discovery before validating required skills. Without
/// this, ordinary extension-discovered filesystem skills would be reported
/// missing because validation would run immediately after publishing
/// `session.started`.
#[test]
fn required_skill_validation_waits_for_session_skill_discovery() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.join("runtime")),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
        agents:
          default_role: reviewer
          role_groups:
            custom:
              roles:
                reviewer:
                  required_skills: [delayed-required-skill]
        "#,
    )
    .expect("write harness config");

    let h = Harness::new_with_provider(
        state_dir,
        dirs,
        echo_runner,
        vec![crate::harness::InProcessTool {
            name: "delayed-skill",
            runner: delayed_skill_runner,
        }],
        "s1",
        tau_proto::SessionStartReason::Initial,
        tau_core::SessionPersistenceMode::Durable,
    )
    .expect("harness should wait for delayed skill and keep reviewer enabled");

    assert!(h.available_roles.contains_key("reviewer"));
    assert_eq!(h.selected_role, "reviewer");
}

/// A misspelled startup default must be visible instead of silently selecting a
/// different role. The harness falls back to the first configured role so users
/// still get a usable session.
#[test]
fn missing_default_role_emits_mandatory_warning_notice_and_falls_back() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            agents: {
                default_role: "ghost",
            },
        }"#,
    )
    .expect("write harness config");

    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("harness");
    assert_eq!(h.selected_role, "engineer-junior");
    let message = find_mandatory_warning_notice(&h, "default_role `ghost`")
        .expect("expected mandatory warning HarnessNotice about missing default_role");
    assert!(
        message.contains("selected `engineer-junior` instead"),
        "message should name the fallback role, got: {message}"
    );
}

/// Provider snapshots are the only source for effort choices. The harness
/// should expose exactly what the provider published and report no choices for
/// unknown models rather than reviving config-derived defaults.
#[test]
fn efforts_for_model_uses_provider_snapshot_levels() {
    use tau_proto::Effort as L;

    let custom: ModelId = "openai/gpt-5.4-pro".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: custom.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![L::Medium, L::High, L::XHigh],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 8_192,
            efforts: vec![L::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);

    assert_eq!(
        efforts_for_model(&provider_models, &custom),
        vec![L::Medium, L::High, L::XHigh],
    );
    assert_eq!(efforts_for_model(&provider_models, &local), vec![L::Off],);
    assert!(
        efforts_for_model(
            &provider_models,
            &"openai/unknown-id".parse().expect("model id"),
        )
        .is_empty(),
        "unknown model yields no provider-published choices",
    );
}

/// `clamp_effort` must degrade `Max` through `XHigh` then `High`, and degrade
/// `XHigh` to `High`, rather than silently dropping supported requests to
/// `Off`.
/// `Off` remains the fallback for other unsupported levels so users with a
/// no-reasoning provider don't get pinned to a level the model can't handle.
#[test]
fn clamp_effort_degrades_max_and_xhigh_when_unsupported() {
    use tau_proto::Effort as L;
    let without_xhigh = [L::Off, L::Minimal, L::Low, L::Medium, L::High];

    assert_eq!(clamp_effort(L::XHigh, &without_xhigh), L::High);
    assert_eq!(clamp_effort(L::Max, &[L::Off, L::High, L::XHigh]), L::XHigh);
    assert_eq!(clamp_effort(L::Max, &without_xhigh), L::High);
    // Sanity: when xhigh IS allowed, no demotion.
    let with_xhigh = [L::Off, L::Minimal, L::Low, L::Medium, L::High, L::XHigh];
    assert_eq!(clamp_effort(L::XHigh, &with_xhigh), L::XHigh);
    // Other unsupported requests still fall to Off.
    assert_eq!(clamp_effort(L::Minimal, &[L::Off]), L::Off);
    // No Off in the allowed set: degrade to the first entry.
    assert_eq!(clamp_effort(L::High, &[L::Medium, L::Low]), L::Medium);
}

/// Verbosity choices come from the provider snapshot. Providers that do not
/// support the knob publish a single fixed level, and unknown models expose no
/// levels.
#[test]
fn verbosities_for_model_uses_provider_snapshot_levels() {
    use tau_proto::Verbosity as V;

    let gpt: ModelId = "openai/gpt-5".parse().expect("model id");
    let locked: ModelId = "openai/gpt-5-locked".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: gpt.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![V::Low, V::Medium, V::High],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: locked.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![V::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);

    assert_eq!(
        verbosities_for_model(&provider_models, &gpt),
        vec![V::Low, V::Medium, V::High],
    );
    assert_eq!(
        verbosities_for_model(&provider_models, &locked),
        vec![V::Medium],
    );
    assert!(
        verbosities_for_model(
            &provider_models,
            &"local/missing".parse().expect("model id"),
        )
        .is_empty(),
    );
}

/// Thinking-summary choices come from the provider snapshot, so the harness no
/// longer consults provider compatibility flags in config.
#[test]
fn thinking_summaries_for_model_uses_provider_snapshot_levels() {
    use tau_proto::ThinkingSummary as T;

    let gpt: ModelId = "openai/gpt-5".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: gpt.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![T::Off, T::Auto, T::Concise, T::Detailed],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: vec![],
            default_affinity: 0,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![T::Off],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
        },
    ]);

    assert_eq!(
        thinking_summaries_for_model(&provider_models, &gpt),
        vec![T::Off, T::Auto, T::Concise, T::Detailed],
    );
    assert_eq!(
        thinking_summaries_for_model(&provider_models, &local),
        vec![T::Off],
    );
}

/// Runtime role updates become the active role definition, then clamp against
/// provider-owned metadata for the resolved model.
#[test]
fn selected_params_use_runtime_role_fields() {
    let model: ModelId = "openai/gpt-5".parse().expect("model id");
    let roles = std::collections::HashMap::from([(
        "engineer".to_owned(),
        tau_config::settings::AgentRole {
            model: Some(model.clone()),
            effort: Some(Effort::High),
            verbosity: Some(Verbosity::Low),
            thinking_summary: Some(ThinkingSummary::Concise),
            ..Default::default()
        },
    )]);
    let selected_role = "engineer";

    let provider_models = provider_models([ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: vec![],
        default_affinity: 0,
        context_window: 128_000,
        efforts: vec![Effort::Off, Effort::Low, Effort::High],
        verbosities: vec![Verbosity::Low, Verbosity::High],
        thinking_summaries: vec![
            ThinkingSummary::Off,
            ThinkingSummary::Auto,
            ThinkingSummary::Concise,
        ],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
    }]);

    let params = selected_params_for_role(&provider_models, &roles, selected_role, &model);
    assert_eq!(params.effort, Effort::High);
    assert_eq!(params.verbosity, Verbosity::Low);
    assert_eq!(params.thinking_summary, ThinkingSummary::Concise);
}
