//! Tests for agent metadata behavior.

use super::*;

/// Agent ids are minted once per conversation as role-prefixed hex strings and
/// are removed from the reverse lookup when the conversation is torn down.
#[test]
fn agent_id_generation_is_stable_and_cleaned_up() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let observer = connect_test_client(&mut h, "stats-observer", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("stats-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    observer.lock().expect("observer frames").clear();

    let first = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let second = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    assert_eq!(first, second);
    assert!(
        drain_stats_updated(&observer).is_empty(),
        "already-loaded public-id lookups must be stats-silent"
    );
    assert_role_hex_agent_id(&first, "engineer");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .get(first.as_str()),
        Some(&cid)
    );

    h.remove_agent(&cid);
    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(first.as_str())
    );

    h.shutdown().expect("shutdown");
}

/// First public-id assignment still publishes the newly loaded agent's initial
/// complete stats snapshot.
#[test]
fn first_agent_id_mint_publishes_initial_stats() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let cid = crate::parse_agent_id("unidentified-runtime");
    let mut agent = Agent::new(
        cid.clone(),
        1,
        h.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    agent.identity.role = Some(h.config.selected_role.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .insert(cid.clone(), agent);
    let observer = connect_test_client(&mut h, "mint-stats-observer", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("mint-stats-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    observer.lock().expect("observer frames").clear();

    let public_id = h.ensure_agent_id_for_agent(&cid).expect("mint agent id");

    let stats = drain_stats_updated(&observer);
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].agent_id, public_id);
    h.shutdown().expect("shutdown");
}

/// Resolving an existing public id while introducing its runtime to the
/// current session still publishes the first loaded stats snapshot.
#[test]
fn first_existing_agent_id_load_publishes_initial_stats() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let public_id = tau_proto::AgentId::parse("existing-runtime").expect("agent id");
    h.append_direct_agent_semantic_event(
        public_id.as_str(),
        tau_core::AgentEventParent::InheritHead,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),
            agent_id: public_id.clone(),
            parent_agent: None,
            role: h.config.selected_role.clone(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    )
    .expect("seed existing identity");
    let cid = crate::parse_agent_id(public_id.as_str());
    let mut agent = Agent::new(
        cid.clone(),
        1,
        h.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    agent.identity.agent_id = Some(public_id.clone());
    agent.identity.role = Some(h.config.selected_role.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .insert(cid.clone(), agent);
    let observer = connect_test_client(&mut h, "load-stats-observer", tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("load-stats-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    observer.lock().expect("observer frames").clear();

    let resolved = h
        .ensure_agent_id_for_agent(&cid)
        .expect("resolve existing agent id");

    assert_eq!(resolved, public_id);
    let stats = drain_stats_updated(&observer);
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].agent_id, public_id);
    h.shutdown().expect("shutdown");
}

#[test]
fn agent_metadata_validation_rejects_bad_key_size_value_and_unknown_target() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let agent_id = tau_proto::AgentId::parse("metadata-target").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());

    let valid = tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("ok"),
        value: CborValue::Text("value".to_owned()),
        mutation_id: None,
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

    for key in [
        path_crate_harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY,
        path_crate_harness::subagents_tool::BOOTSTRAP_PROMPT_AGENT_METADATA_KEY,
    ] {
        let reserved_key = tau_proto::AgentMetadataKey::new(key);
        let reserved_set = tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: reserved_key.clone(),
            value: CborValue::Bool(false),
            mutation_id: None,
            inheritable: false,
        };
        assert!(
            h.validate_agent_metadata_set(&reserved_set)
                .expect_err("reserved set rejected")
                .contains("reserved")
        );
        assert!(
            h.validate_agent_metadata_unset(&tau_proto::AgentMetadataUnset {
                agent_id: agent_id.clone(),
                key: reserved_key.clone(),
            })
            .expect_err("reserved unset rejected")
            .contains("reserved")
        );
        assert!(
            h.validate_initial_agent_metadata(&[tau_proto::AgentInitialMetadata {
                key: reserved_key,
                value: CborValue::Bool(true),
                inheritable: false,
            }])
            .expect_err("reserved initial metadata rejected")
            .contains("reserved")
        );
    }

    h.shutdown().expect("shutdown");
}

/// An explicit-parent typed start inherits only eligible metadata, then returns
/// exactly one result and detaches into a loaded ordinary worker. A fresh user
/// turn must preserve membership without reviving the completed request.
#[test]
fn explicit_parent_typed_start_inherits_metadata_and_remains_loaded_after_completion() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let result_frames = connect_test_tool(&mut h, "conn-delegate");
    h.submit_user_prompt(test_session_id("s1"), "parent prompt".to_owned())
        .expect("submit parent");
    let parent_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|conversation| conversation.identity.agent_id.clone())
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
                mutation_id: None,
                inheritable,
            }),
        );
    }

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
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
    let child_agent_id = durable_agent_id_for_conversation(&h, &child_cid);

    let child_events = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(child_agent_id.as_str())
        .expect("child events");
    assert!(child_events.iter().any(|entry| matches!(
        &entry.event,
        Event::AgentMetadataSet(set)
            if set.agent_id == child_agent_id
                && set.key == inherit_key
                && set.value == CborValue::Text("inherited".to_owned())
                && set.inheritable
    )));
    assert!(child_events.iter().all(|entry| !matches!(
        &entry.event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "local-key"
    )));

    let child_prompt_id = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, cid)| (cid == &child_cid).then_some(prompt_id.clone()))
        .expect("child prompt");
    let mut response =
        provider_text_response(&child_prompt_id, child_agent_id.clone(), "side result");
    response.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("conn-delegate"),
        query_id: "q-inherit".to_owned(),
    };
    h.handle_provider_response_finished(response)
        .expect("complete explicit-parent child");

    let child = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&child_cid)
        .expect("completed child retained");
    assert!(child.identity.originator.is_user());
    assert!(child.identity.source_connection.is_none());
    assert!(child.identity.parent_tool_call_id.is_none());
    assert!(child.identity.parent_agent_id.is_none());
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&child_agent_id),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto)
    );
    assert_eq!(
        result_frames
            .lock()
            .expect("result frames")
            .iter()
            .filter(|frame| matches!(
                peel_inner_event(&frame.frame),
                Some(Event::StartAgentResult(result)) if result.query_id == "q-inherit"
            ))
            .count(),
        1
    );

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "fresh child turn".to_owned(),
            agent_id: child_agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("submit fresh child turn");
    let fresh_prompt_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&child_cid)
        .and_then(|child| child.dispatch.in_flight_prompt.clone())
        .expect("fresh child prompt");
    h.handle_provider_response_finished(provider_text_response(
        &fresh_prompt_id,
        child_agent_id.clone(),
        "fresh result",
    ))
    .expect("complete fresh child turn");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&child_cid)
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&child_agent_id)
    );
    assert_eq!(
        result_frames
            .lock()
            .expect("result frames")
            .iter()
            .filter(|frame| matches!(
                peel_inner_event(&frame.frame),
                Some(Event::StartAgentResult(result)) if result.query_id == "q-inherit"
            ))
            .count(),
        1,
        "a fresh user turn must not complete the old start request again"
    );
    let session_events = h
        .session_runtime
        .store
        .session_events("s1")
        .expect("session events");
    assert_eq!(
        session_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::SessionAgentLoaded(loaded)
                    if loaded.agent_id == child_agent_id
            ))
            .count(),
        1
    );
    assert!(session_events.iter().all(|record| !matches!(
        &record.event,
        Event::SessionAgentUnloaded(unloaded)
            if unloaded.agent_id == child_agent_id
    )));

    h.shutdown().expect("shutdown");
}

/// Explicit UI child metadata must override a colliding inherited entry as a
/// whole while noncolliding inheritable parent metadata remains copied durably.
#[test]
fn ui_child_metadata_overrides_colliding_parent_entry() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "parent prompt".to_owned())
        .expect("submit parent");
    let parent_cid = test_user_agent(&h);
    let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
    let collision_key = tau_proto::AgentMetadataKey::new("collision-key");
    let inherited_key = tau_proto::AgentMetadataKey::new("inherited-key");
    for (key, value) in [
        (collision_key.clone(), "parent collision"),
        (inherited_key.clone(), "parent inherited"),
    ] {
        h.publish_event(
            None,
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: parent_agent_id.clone(),
                key,
                value: CborValue::Text(value.to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
        );
    }
    let existing_agent_ids: std::collections::HashSet<_> = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .filter_map(|agent| agent.identity.agent_id.clone())
        .collect();

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "metadata-override-create".to_owned(),
            literal: false,
            parent_agent: Some(parent_agent_id),
            session_id: test_session_id("s1"),
            role: h.config.selected_role.clone(),
            model_override: None,
            effort_override: None,
            metadata: vec![tau_proto::AgentInitialMetadata {
                key: collision_key.clone(),
                value: CborValue::Text("child explicit".to_owned()),
                inheritable: false,
            }],
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            ephemeral: false,
        },
    )
    .expect("create child");

    let child_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .filter_map(|agent| agent.identity.agent_id.clone())
        .find(|agent_id| !existing_agent_ids.contains(agent_id))
        .expect("new child agent id");
    let child_events = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(child_agent_id.as_str())
        .expect("child events");
    assert!(child_events.iter().any(|record| matches!(
        &record.event,
        Event::AgentStarted(started)
            if started.metadata.iter().any(|metadata|
                metadata.key == collision_key
                    && metadata.value == CborValue::Text("child explicit".to_owned())
                    && !metadata.inheritable)
    )));
    assert!(child_events.iter().all(|record| !matches!(
        &record.event,
        Event::AgentMetadataSet(set) if set.key == collision_key
    )));
    assert!(child_events.iter().any(|record| matches!(
        &record.event,
        Event::AgentMetadataSet(set)
            if set.key == inherited_key
                && set.value == CborValue::Text("parent inherited".to_owned())
                && set.inheritable
    )));

    let replayed = h
        .session_runtime
        .agent_store
        .load_agent(child_agent_id.as_str())
        .expect("load child")
        .expect("replayed child");
    assert_eq!(
        replayed.metadata().get(&collision_key),
        Some(&tau_core::AgentMetadataEntry {
            value: CborValue::Text("child explicit".to_owned()),
            inheritable: false,
        })
    );
    assert_eq!(
        replayed.metadata().get(&inherited_key),
        Some(&tau_core::AgentMetadataEntry {
            value: CborValue::Text("parent inherited".to_owned()),
            inheritable: true,
        })
    );

    h.shutdown().expect("shutdown");
}

/// A manually created agent has no explicit task or `:name`, so the durable
/// start fact must not synthesize its role as presentation metadata.
#[test]
fn manually_created_agent_has_no_default_display_name() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer-junior");
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.agent_id.as_str() == cid.as_str() => {
                Some(started)
            }
            _ => None,
        })
        .expect("manual agent start fact");

    assert_eq!(started.role, "engineer-junior");
    assert_eq!(started.display_name, None);
    let conversation = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("manual agent conversation");
    assert!(conversation.identity.display_name.is_none());

    h.shutdown().expect("shutdown");
}

/// Explicit names remain authoritative even when their text equals the agent's
/// role, and restoration must not mistake them for an old synthesized default.
#[test]
fn explicit_display_name_equal_to_role_survives_restore() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer-junior");
        let agent_id = crate::parse_agent_id(cid.as_str());
        let observer =
            connect_test_client(&mut h, "display-name-observer", tau_proto::ClientKind::Ui);
        h.runtime_io
            .bus
            .set_subscriptions(
                &crate::test_connection_id("display-name-observer"),
                Vec::new(),
                vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_DISPLAY_NAME_SET,
                )],
            )
            .expect("observer subscription");
        observer.lock().expect("observer frames").clear();

        h.handle_ui_set_agent_display_name(
            crate::harness::harness_connection_id(),
            tau_proto::UiSetAgentDisplayName {
                session_id: h.session_runtime.current_session_id.clone(),
                agent_id: agent_id.clone(),
                display_name: "  engineer-junior  ".to_owned(),
            },
        )
        .expect("set explicit display name");
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|conversation| conversation.identity.display_name.as_deref()),
            Some("engineer-junior")
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent(agent_id.as_str())
                .expect("agent tree")
                .display_name(),
            Some("engineer-junior")
        );
        assert_eq!(
            observer
                .lock()
                .expect("observer frames")
                .iter()
                .filter_map(|frame| peel_inner_event(&frame.frame))
                .filter_map(|event| match event {
                    Event::AgentDisplayNameSet(name) => Some(name.display_name.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["engineer-junior"]
        );
        h.shutdown().expect("shutdown");
        agent_id
    };

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(agent_id.as_str())
        .expect("restored agent route");
    assert_eq!(
        resumed
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conversation| conversation.identity.display_name.as_deref()),
        Some("engineer-junior")
    );

    resumed.shutdown().expect("shutdown");
}

/// A rejected display-name admission must leave live and canonical state at the
/// prior name, must not catch up later, and must not block a fresh rename.
#[test]
fn display_name_admission_full_has_no_eager_projection_or_retry() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let session_id = h.session_runtime.current_session_id.clone();
    let rename = |h: &mut Harness, display_name: &str| {
        h.handle_ui_set_agent_display_name(
            crate::harness::harness_connection_id(),
            tau_proto::UiSetAgentDisplayName {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                display_name: display_name.to_owned(),
            },
        )
        .expect("rename request");
    };

    rename(&mut h, "Alpha");
    let committed_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentDisplayNameSet(_)))
        .count();
    reject_next_semantic_admission(&h);
    rename(&mut h, "Beta");

    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .as_deref(),
        Some("Alpha")
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .display_name(),
        Some("Alpha")
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentDisplayNameSet(_)))
            .count(),
        committed_before,
        "the rejected rename must not enter canonical observations"
    );

    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable persistence owner")
        .signal_capacity_ready_for_test();
    rename(&mut h, "  Gamma  ");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .as_deref(),
        Some("Gamma")
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .display_name(),
        Some("Gamma")
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter_map(|event| match event {
                Event::AgentDisplayNameSet(name) => Some(name.display_name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["Alpha", "Gamma"],
        "capacity recovery must not retry the rejected Beta rename"
    );
    h.shutdown().expect("shutdown");
}

/// Committed display-name projection must use the current route and reject
/// stale runtime identity or session incarnations without reviving anything.
#[test]
fn committed_display_name_projection_targets_only_matching_current_runtime() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let committed = tau_proto::AgentDisplayNameSet {
        agent_id: agent_id.clone(),
        display_name: "  Canonical  ".to_owned(),
    };

    h.agent_runtime
        .agent_registry
        .agent_routes
        .remove(&agent_id);
    h.project_committed_agent_display_name(&committed);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .is_none()
    );

    let absent_cid = crate::parse_agent_id("absent-runtime");
    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(agent_id.clone(), absent_cid.clone());
    h.project_committed_agent_display_name(&committed);
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&absent_cid),
        "projection must not revive a route whose runtime is absent"
    );

    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(agent_id.clone(), cid.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("current runtime")
        .identity
        .agent_id = Some(tau_proto::AgentId::parse("stale-agent").expect("agent id"));
    h.project_committed_agent_display_name(&committed);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .is_none()
    );

    let current_session = h.session_runtime.current_session_id.clone();
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("current runtime");
    agent.identity.agent_id = Some(agent_id.clone());
    agent.identity.session_id = tau_proto::SessionId::parse("stale-session").expect("session id");
    h.project_committed_agent_display_name(&committed);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .is_none()
    );

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("current runtime")
        .identity
        .session_id = current_session;
    h.project_committed_agent_display_name(&committed);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .display_name
            .as_deref(),
        Some("Canonical")
    );
    h.shutdown().expect("shutdown");
}

/// A role-derived name written by a custom template is durable data. Resuming
/// under the newer built-in template must preserve it rather than guessing that
/// text equal to a role was synthetic.
#[test]
fn custom_role_display_name_survives_restore_under_built_in_template() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.agent_display_name_template = Some("{{role}}".to_owned());
        let cid = h.create_durable_user_agent(test_session_id("s1"), "engineer-junior");
        let started = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStarted(started) if started.agent_id.as_str() == cid.as_str() => {
                    Some(started)
                }
                _ => None,
            })
            .expect("custom-template agent start fact");
        assert_eq!(started.display_name.as_deref(), Some("engineer-junior"));
        h.shutdown().expect("shutdown");
        started.agent_id
    };

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume under built-in template");
    let cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(agent_id.as_str())
        .expect("restored agent route");
    assert_eq!(
        resumed
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("restored agent conversation")
            .identity
            .display_name
            .as_deref(),
        Some("engineer-junior")
    );

    resumed.shutdown().expect("shutdown");
}
