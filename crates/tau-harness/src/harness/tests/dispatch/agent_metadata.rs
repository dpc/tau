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

    let first = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    let second = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    assert_eq!(first, second);
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
        .agent_events(child_agent_id.as_str())
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

        h.handle_ui_set_agent_display_name(
            crate::harness::harness_connection_id(),
            tau_proto::UiSetAgentDisplayName {
                session_id: h.session_runtime.current_session_id.clone(),
                agent_id: agent_id.clone(),
                display_name: "engineer-junior".to_owned(),
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
