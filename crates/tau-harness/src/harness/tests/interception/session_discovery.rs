//! Atomic discovery snapshot regressions.

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

fn skill(
    path: &std::path::Path,
    name: &str,
    modified: Option<i64>,
) -> tau_proto::DiscoverySkillCandidate {
    std::fs::write(
        path,
        format!("---\nname: {name}\ndescription: {name}\n---\nbody {name}\n"),
    )
    .expect("write skill");
    tau_proto::DiscoverySkillCandidate {
        name: name.into(),
        description: format!("{name} description"),
        file_path: path.to_path_buf(),
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: Some("[args]".to_owned()),
        sampled_modified: modified.map(tau_proto::DiscoveryModifiedMicros::new),
    }
}

fn snapshot(
    skills: Vec<tau_proto::DiscoverySkillCandidate>,
    agents_files: Vec<tau_proto::DiscoveryAgentsFile>,
) -> tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
    tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        skills,
        agents_files,
    }
}

fn connect_snapshot_interceptor(h: &mut Harness, selector: tau_proto::EventName) {
    connect_test_tool(h, "snapshot-interceptor");
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(selector)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register snapshot interceptor");
}

fn source_committed(h: &Harness, source: &str, predicate: impl Fn(&Event) -> bool) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

/// A complete source replacement supports every positive and removal mutation
/// without exposing intermediate source state.
#[test]
fn complete_source_snapshot_atomically_adds_updates_deletes_renames_and_clears() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let one = tmp.path().join("one.md");
    let two = tmp.path().join("two.md");

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(vec![skill(&one, "one", Some(1))], vec![]),
    );
    assert!(h.context_discovery.skills.contains_key("one"));

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(vec![skill(&two, "two", Some(2))], vec![]),
    );
    assert!(!h.context_discovery.skills.contains_key("one"));
    assert!(h.context_discovery.skills.contains_key("two"));
    assert_eq!(
        h.context_discovery.skills[&tau_proto::SkillName::new("two")]
            .argument_hint
            .as_deref(),
        Some("[args]")
    );

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(vec![], vec![]),
    );
    assert!(!h.context_discovery.skills.contains_key("two"));
}

/// Equal timestamps preserve insertion order and source removal reveals the
/// next collision candidate.
#[test]
fn collision_update_and_source_clear_fall_back_with_stable_equal_mtime_order() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let first = tmp.path().join("first.md");
    let second = tmp.path().join("second.md");

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("first"),
        snapshot(vec![skill(&first, "same", Some(7))], vec![]),
    );
    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("second"),
        snapshot(vec![skill(&second, "same", Some(7))], vec![]),
    );
    assert_eq!(
        h.context_discovery.skills[&tau_proto::SkillName::new("same")]
            .source
            .file_path(),
        Some(first.as_path())
    );

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("first"),
        snapshot(vec![], vec![]),
    );
    assert_eq!(
        h.context_discovery.skills[&tau_proto::SkillName::new("same")]
            .source
            .file_path(),
        Some(second.as_path())
    );
}

/// Invalid and duplicate items are omitted while valid siblings still replace
/// the source atomically.
#[test]
fn invalid_and_duplicate_items_are_omitted_without_rejecting_valid_replacement() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let valid = tmp.path().join("valid.md");
    let duplicate = tmp.path().join("duplicate.md");
    let invalid = tmp.path().join("invalid.md");
    let agents = tmp.path().join("AGENTS.md");
    let canonical_duplicate = tmp.path().join(".").join("AGENTS.md");

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(
            vec![
                skill(&valid, "valid", None),
                skill(&duplicate, "valid", Some(9)),
                skill(&invalid, "invalid name", None),
            ],
            vec![
                tau_proto::DiscoveryAgentsFile {
                    file_path: agents.clone(),
                    content: "first".to_owned(),
                },
                tau_proto::DiscoveryAgentsFile {
                    file_path: canonical_duplicate,
                    content: "duplicate".to_owned(),
                },
            ],
        ),
    );

    assert!(h.context_discovery.skills.contains_key("valid"));
    assert!(!h.context_discovery.skills.contains_key("invalid name"));
    assert_eq!(h.context_discovery.agents_files.len(), 1);
    assert_eq!(h.context_discovery.agents_files[0].content, "first");
}

/// AGENTS.md ordering follows the complete producer snapshot and an empty
/// replacement clears the source.
#[test]
fn agents_files_keep_declared_order_and_empty_snapshot_removes_them() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let broad = tmp.path().join("AGENTS.md");
    let nested_dir = tmp.path().join("nested");
    std::fs::create_dir(&nested_dir).expect("nested");
    let nested = nested_dir.join("AGENTS.md");
    std::fs::write(&broad, "broad").expect("broad");
    std::fs::write(&nested, "nested").expect("nested");

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(
            vec![],
            vec![tau_proto::DiscoveryAgentsFile {
                file_path: nested.clone(),
                content: "old nested".to_owned(),
            }],
        ),
    );
    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(
            vec![],
            vec![
                tau_proto::DiscoveryAgentsFile {
                    file_path: broad.clone(),
                    content: "broad".to_owned(),
                },
                tau_proto::DiscoveryAgentsFile {
                    file_path: nested.clone(),
                    content: "nested".to_owned(),
                },
            ],
        ),
    );
    assert_eq!(
        h.context_discovery.agents_files
            .iter()
            .map(|file| file.file_path.clone())
            .collect::<Vec<_>>(),
        vec![
            broad.canonicalize().expect("canonical broad AGENTS path"),
            nested.canonicalize().expect("canonical nested AGENTS path")
        ]
    );

    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("source"),
        snapshot(vec![], vec![]),
    );
    assert!(h.context_discovery.agents_files.is_empty());
}

/// A snapshot for another session cannot change the current baseline.
#[test]
fn wrong_session_snapshot_is_effect_free() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("wrong.md");
    let mut wrong = snapshot(vec![skill(&path, "wrong", None)], vec![]);
    wrong.session_id = "other"
        .parse::<tau_proto::SessionId>()
        .expect("known-safe SessionId must be valid");
    h.apply_session_discovery_snapshot(&crate::test_connection_id("source"), wrong);
    assert!(!h.context_discovery.skills.contains_key("wrong"));
}

/// Late UIs receive canonical current state without raw declarations or prompt
/// side effects.
#[test]
fn late_ui_gets_one_session_and_one_live_agent_projection_without_raw_declarations() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let _ = ensure_test_user_agent(&mut h);
    let prompt_side_effects_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentUserMessageInjected(_)))
        .count();
    let sink = connect_test_client(&mut h, "late-discovery-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "late-discovery-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::HARNESS_SESSION_SKILLS_AVAILABLE),
                EventSelector::Exact(tau_proto::EventName::HARNESS_AGENT_CONTEXT_INITIALIZED),
                EventSelector::Exact(
                    tau_proto::EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED,
                ),
                EventSelector::Exact(
                    tau_proto::EventName::EXTENSION_AGENT_DISCOVERY_SNAPSHOT_DECLARED,
                ),
            ],
        })),
    )
    .expect("subscribe");

    let events = sink
        .lock()
        .expect("sink")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::HarnessSessionSkillsAvailable(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::HarnessAgentContextInitialized(_)))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ExtensionSessionDiscoverySnapshotDeclared(_)
            | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
    )));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentUserMessageInjected(_)))
            .count(),
        prompt_side_effects_before
    );
}

/// Ordinary publication applies only the committed interceptor replacement and
/// a later dropped snapshot has no state effect.
#[test]
fn session_snapshot_commit_boundary_honors_replace_and_drop() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "snapshot-owner",
        tau_proto::ClientKind::Action,
    );
    connect_test_tool(&mut h, "snapshot-interceptor");
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let original = tmp.path().join("original.md");
    let replacement = tmp.path().join("replacement.md");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
            vec![skill(&original, "original", None)],
            vec![],
        )),
    )
    .expect("park original");
    assert!(!h.context_discovery.skills.contains_key("original"));
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(
                Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
                    vec![skill(&replacement, "replacement", None)],
                    vec![],
                )),
            ))),
        })),
    )
    .expect("replace");
    assert!(!h.context_discovery.skills.contains_key("original"));
    assert!(h.context_discovery.skills.contains_key("replacement"));

    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(vec![], vec![])),
    )
    .expect("park clear");
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop clear");
    assert!(h.context_discovery.skills.contains_key("replacement"));
}

/// Agent snapshots cross ordinary commit but only mutate the exact pending
/// session/agent/initialization tuple.
#[test]
fn agent_snapshot_commit_boundary_rejects_wrong_initialization() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "agent-snapshot-owner",
        "agent-snapshot-owner",
        tau_proto::ClientKind::Core,
    );
    let agent_id = tau_proto::AgentId::parse("snapshot-agent").expect("agent");
    let initialization_id = tau_proto::AgentInitializationId::parse("current-init")
        .expect("test identifier must be valid");
    h.context_discovery.pending_agents.insert(
        agent_id.clone(),
        PendingAgentDiscovery {
            initialization_id: initialization_id.clone(),
            skill_candidates: Default::default(),
            skills: Default::default(),
            agents_files: Vec::new(),
            waiting_on: [crate::test_connection_id("agent-snapshot-owner")]
                .into_iter()
                .collect(),
        },
    );
    let path = tmp.path().join("agent.md");
    let event = |initialization_id| {
        Event::ExtensionAgentDiscoverySnapshotDeclared(
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                agent_initialization_id: initialization_id,
                skills: vec![skill(&path, "agent-skill", None)],
                agents_files: Vec::new(),
            },
        )
    };
    h.handle_extension_event_inner(
        &crate::test_connection_id("agent-snapshot-owner"),
        event(
            tau_proto::AgentInitializationId::parse("stale-init")
                .expect("test identifier must be valid"),
        ),
    )
    .expect("stale");
    assert!(h.context_discovery.pending_agents[&agent_id].skills.is_empty());
    h.handle_extension_event_inner(
        &crate::test_connection_id("agent-snapshot-owner"),
        event(initialization_id),
    )
    .expect("current");
    assert!(
        h.context_discovery.pending_agents[&agent_id]
            .skills
            .contains_key("agent-skill")
    );
}

/// A pre-Ready session snapshot holds its activation reservation through a
/// delayed interception and installs state only after the committed reply.
#[test]
fn pre_ready_session_snapshot_waits_for_commit_before_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Action,
    );
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED,
    );
    h.extensions
        .entries
        .get_mut("snapshot-owner")
        .expect("snapshot owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let path = tmp.path().join("startup.md");

    h.handle_extension_event(
        "snapshot-owner",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
                vec![skill(&path, "startup", None)],
                Vec::new(),
            ))),
            persist: false,
        })),
    )
    .expect("park startup snapshot");
    assert!(h.publication.pending_intercept.is_some());
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("snapshot-owner"),
        Some(&1)
    );
    h.handle_extension_message(
        &crate::test_connection_id("snapshot-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("record ready");

    assert_eq!(
        h.extensions.entries["snapshot-owner"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("snapshot-owner"),
        Some(&1)
    );
    assert!(!h.context_discovery.skills.contains_key("startup"));

    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit startup snapshot");

    assert_eq!(
        h.extensions.entries["snapshot-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_session_discovery_declarations
            .contains_key("snapshot-owner")
    );
    assert!(source_committed(&h, "snapshot-owner", |event| matches!(
        event,
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot)
            if snapshot.skills.iter().any(|skill| skill.name.as_str() == "startup")
    )));
}

/// Agent snapshot replacement and drop remain effect-free until commit and
/// preserve the last committed source snapshot when a later update is dropped.
#[test]
fn agent_snapshot_delayed_replace_and_drop_obey_commit_boundary() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Core,
    );
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::EXTENSION_AGENT_DISCOVERY_SNAPSHOT_DECLARED,
    );
    let agent_id = tau_proto::AgentId::parse("snapshot-agent").expect("agent");
    let initialization_id =
        tau_proto::AgentInitializationId::parse("init").expect("test identifier must be valid");
    h.context_discovery.pending_agents.insert(
        agent_id.clone(),
        PendingAgentDiscovery {
            initialization_id: initialization_id.clone(),
            skill_candidates: Default::default(),
            skills: Default::default(),
            agents_files: Vec::new(),
            waiting_on: [crate::test_connection_id("snapshot-owner")]
                .into_iter()
                .collect(),
        },
    );
    let original = tmp.path().join("original.md");
    let replacement = tmp.path().join("replacement.md");
    let event = |candidate| {
        Event::ExtensionAgentDiscoverySnapshotDeclared(
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                agent_initialization_id: initialization_id.clone(),
                skills: candidate,
                agents_files: Vec::new(),
            },
        )
    };

    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        event(vec![skill(&original, "original", None)]),
    )
    .expect("park original");
    assert!(h.context_discovery.pending_agents[&agent_id].skills.is_empty());
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(event(vec![skill(
                &replacement,
                "replacement",
                None,
            )])))),
        })),
    )
    .expect("commit replacement");
    assert!(
        h.context_discovery.pending_agents[&agent_id]
            .skills
            .contains_key("replacement")
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        event(Vec::new()),
    )
    .expect("park clear");
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop clear");
    assert!(
        h.context_discovery.pending_agents[&agent_id]
            .skills
            .contains_key("replacement")
    );
}

/// Disconnecting a configured publisher while its snapshot is parked makes
/// the eventual old-generation commit observational and clears its source.
#[test]
fn disconnected_snapshot_generation_cannot_mutate_discovery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "old-generation",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED,
    );
    let stale = tmp.path().join("stale.md");
    h.handle_extension_event_inner(
        &crate::test_connection_id("old-generation"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
            vec![skill(&stale, "stale", None)],
            Vec::new(),
        )),
    )
    .expect("park stale snapshot");

    h.handle_disconnect(&crate::test_connection_id("old-generation"));
    connect_ready_configured_extension(
        &mut h,
        "new-generation",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale snapshot");

    assert!(!h.context_discovery.skills.contains_key("stale"));
}

/// Unloading an agent while its configured snapshot is parked prevents the
/// eventual committed observation from recreating pending discovery state.
#[test]
fn unloaded_agent_cannot_be_recreated_by_parked_snapshot() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_message(
        &crate::test_connection_id("snapshot-owner"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe to agent loads");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
    )
    .expect("register context provider");
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::EXTENSION_AGENT_DISCOVERY_SNAPSHOT_DECLARED,
    );
    let cid = h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.ensure_loaded_agent_for_agent(&cid, agent_id.as_str());
    let initialization_id = h.context_discovery.pending_agents[&agent_id]
        .initialization_id
        .clone();
    let path = tmp.path().join("unloaded.md");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionAgentDiscoverySnapshotDeclared(
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                agent_initialization_id: initialization_id,
                skills: vec![skill(&path, "unloaded", None)],
                agents_files: Vec::new(),
            },
        ),
    )
    .expect("park agent snapshot");
    let unloaded = Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id: agent_id.clone(),
    });
    h.react_to_committed_event(None, &unloaded, true, None);
    assert!(!h.context_discovery.pending_agents.contains_key(&agent_id));

    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit snapshot after unload");
    assert!(!h.context_discovery.pending_agents.contains_key(&agent_id));
    assert!(!h.context_discovery.frozen_agents.contains_key(&agent_id));
}

/// An oversized pre-Ready replacement fails activation accounting and releases
/// the session-discovery reservation without exposing snapshot state.
#[test]
fn oversized_startup_snapshot_replacement_fails_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.extensions.initial_tool_preflight_complete = false;
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED,
    );
    h.extensions
        .entries
        .get_mut("snapshot-owner")
        .expect("snapshot owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let path = tmp.path().join("small.md");
    h.handle_extension_event(
        "snapshot-owner",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
                vec![skill(&path, "small", None)],
                Vec::new(),
            ))),
            persist: false,
        })),
    )
    .expect("park startup snapshot");
    let mut oversized = snapshot(Vec::new(), Vec::new());
    oversized.agents_files.push(tau_proto::DiscoveryAgentsFile {
        file_path: tmp.path().join("AGENTS.md"),
        content: "x".repeat(crate::harness::MAX_EXTENSION_ACTIVATION_BYTES),
    });

    let result = h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(
                Event::ExtensionSessionDiscoverySnapshotDeclared(oversized),
            ))),
        })),
    );
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.to_string().contains("activation staging exceeds"))
            || h.extensions.entries["snapshot-owner"].state
                == crate::extension::ExtensionState::Disconnected
    );
    assert!(
        !h.extensions
            .pending_session_discovery_declarations
            .contains_key("snapshot-owner")
    );
    assert!(!h.context_discovery.skills.contains_key("small"));
}

/// Two simultaneous initializations retain independent snapshot/ready
/// correlation; duplicate snapshots replace in place and a snapshot arriving
/// after readiness cannot reopen or mutate a finalized initialization.
#[test]
fn concurrent_agents_isolate_duplicate_and_ready_before_snapshot() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_message(
        &crate::test_connection_id("snapshot-owner"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe to agent loads");
    h.handle_extension_event(
        "snapshot-owner",
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("register context provider");
    assert!(
        h.context_discovery.agent_context_providers.contains(
            &tau_proto::ConnectionId::parse("snapshot-owner")
                .expect("test connection id must satisfy the identifier grammar")
        )
    );
    let first_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let second_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let first = durable_agent_id_for_conversation(&h, &first_cid);
    let second = durable_agent_id_for_conversation(&h, &second_cid);
    h.ensure_loaded_agent_for_agent(&first_cid, first.as_str());
    h.ensure_loaded_agent_for_agent(&second_cid, second.as_str());
    let first_init = h.context_discovery.pending_agents[&first].initialization_id.clone();
    let second_init = h.context_discovery.pending_agents[&second].initialization_id.clone();
    let first_path = tmp.path().join("first.md");
    let late_path = tmp.path().join("late.md");
    let event = |agent_id: tau_proto::AgentId,
                 initialization_id: tau_proto::AgentInitializationId,
                 skills| {
        Event::ExtensionAgentDiscoverySnapshotDeclared(
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id,
                agent_initialization_id: initialization_id,
                skills,
                agents_files: Vec::new(),
            },
        )
    };
    let ready = |agent_id, agent_initialization_id| {
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id,
            agent_initialization_id,
        })
    };

    for _ in 0..2 {
        h.handle_extension_event_inner(
            &crate::test_connection_id("snapshot-owner"),
            event(
                first.clone(),
                first_init.clone(),
                vec![skill(&first_path, "first", None)],
            ),
        )
        .expect("publish duplicate first snapshot");
    }
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        ready(first.clone(), first_init.clone()),
    )
    .expect("finalize first");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        ready(second.clone(), second_init.clone()),
    )
    .expect("finalize second before snapshot");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        event(
            second.clone(),
            second_init,
            vec![skill(&late_path, "late", None)],
        ),
    )
    .expect("publish late second snapshot");

    assert!(
        h.context_discovery.frozen_agents[&first]
            .skills
            .contains_key("first")
    );
    assert!(
        !h.context_discovery.frozen_agents[&second]
            .skills
            .contains_key("late")
    );
    assert!(!h.context_discovery.pending_agents.contains_key(&first));
    assert!(!h.context_discovery.pending_agents.contains_key(&second));
}

/// Configured publication enforces item, aggregate decoded-byte, and
/// per-AGENTS-file bounds while atomically accepting valid siblings.
#[test]
fn configured_snapshot_omits_items_beyond_all_discovery_bounds() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Action,
    );
    let item_path = tmp.path().join("item.md");
    let template = skill(&item_path, "item-0", None);
    let skills = (0..crate::harness::MAX_DISCOVERY_SNAPSHOT_ITEMS + 10_000)
        .map(|index| {
            let mut candidate = template.clone();
            candidate.name = format!("item-{index}").into();
            candidate.description = format!("item {index}");
            candidate
        })
        .collect();
    let notices_before = h.replayable_harness_notices.len();
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(skills, Vec::new())),
    )
    .expect("publish item-bounded snapshot");
    assert!(h.context_discovery.skills.contains_key("item-8191"));
    assert!(!h.context_discovery.skills.contains_key("item-8192"));
    assert_eq!(
        h.replayable_harness_notices.len() - notices_before,
        1,
        "all excess items must share one aggregate truncation notice"
    );

    let oversized_path = tmp.path().join("oversized-description.md");
    let accepted_path = tmp.path().join("accepted-after-bytes.md");
    let mut oversized = skill(&oversized_path, "oversized-description", None);
    oversized.description = "x".repeat(crate::harness::MAX_DISCOVERY_SNAPSHOT_BYTES + 1);
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
            vec![
                oversized,
                skill(&accepted_path, "accepted-after-bytes", None),
            ],
            Vec::new(),
        )),
    )
    .expect("publish byte-bounded snapshot");
    assert!(!h.context_discovery.skills.contains_key("oversized-description"));
    assert!(h.context_discovery.skills.contains_key("accepted-after-bytes"));

    let oversized_agents = tmp.path().join("AGENTS.local.md");
    let accepted_agents = tmp.path().join("AGENTS.md");
    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(
            Vec::new(),
            vec![
                tau_proto::DiscoveryAgentsFile {
                    file_path: oversized_agents,
                    content: "x".repeat(super::super::super::MAX_DISCOVERY_AGENTS_FILE_BYTES + 1),
                },
                tau_proto::DiscoveryAgentsFile {
                    file_path: accepted_agents,
                    content: "accepted".to_owned(),
                },
            ],
        )),
    )
    .expect("publish AGENTS-size-bounded snapshot");
    assert_eq!(h.context_discovery.agents_files.len(), 1);
    assert_eq!(h.context_discovery.agents_files[0].content, "accepted");
}

/// Filling the aggregate byte budget cannot prevent raw-item accounting from
/// reaching the traversal cap or produce diagnostics for the entire frame tail.
#[test]
fn byte_full_snapshot_still_stops_at_raw_item_limit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "snapshot-owner",
        "configured-snapshot-owner",
        tau_proto::ClientKind::Action,
    );
    let path = tmp.path().join("byte-fill.md");
    let mut byte_fill = skill(&path, "byte-fill", None);
    byte_fill.description = "x".repeat(crate::harness::MAX_DISCOVERY_SNAPSHOT_BYTES - 1024);
    let template = skill(&path, "tail-0", None);
    let mut skills = Vec::with_capacity(crate::harness::MAX_DISCOVERY_SNAPSHOT_ITEMS + 10_000);
    skills.push(byte_fill);
    skills.extend(
        (0..crate::harness::MAX_DISCOVERY_SNAPSHOT_ITEMS + 10_000).map(|index| {
            let mut candidate = template.clone();
            candidate.name = format!("tail-{index}").into();
            candidate
        }),
    );
    let notices_before = h.replayable_harness_notices.len();

    h.handle_extension_event_inner(
        &crate::test_connection_id("snapshot-owner"),
        Event::ExtensionSessionDiscoverySnapshotDeclared(snapshot(skills, Vec::new())),
    )
    .expect("publish byte-full snapshot");

    assert!(
        h.replayable_harness_notices.len() - notices_before
            <= super::super::super::MAX_DISCOVERY_SNAPSHOT_ITEMS + 1,
        "validation must inspect only the raw-item cap and one aggregate tail"
    );
}
