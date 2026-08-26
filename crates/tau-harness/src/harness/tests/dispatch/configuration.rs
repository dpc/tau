//! Tests for configuration behavior.

use super::*;

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
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
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
            &crate::test_connection_id("conn-delegate"),
            StartAgentRequest {
                trusted_internal_spans: Vec::new(),
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

/// Production prompt assembly must resolve the configured role-group key before
/// rendering both role fragments and their enclosing custom system template.
#[test]
fn configured_role_group_reaches_fragment_and_system_template_contexts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let role_name = "security-reviewer";
    h.config.available_role_groups = vec![tau_proto::HarnessRoleGroup {
        name: "reviewers".to_owned(),
        roles: vec![role_name.to_owned()],
    }];
    let role = h
        .config
        .available_roles
        .entry(role_name.to_owned())
        .or_default();
    role.prompt_override = Some("role-group-test".to_owned());
    role.prompt_fragments
        .push(tau_config::settings::RolePromptFragment {
            name: "role-group-fragment".to_owned(),
            priority: tau_proto::PromptPriority::new(100),
            text: tau_proto::PromptContent::new("FRAGMENT {{role.group}}/{{role.name}}"),
        });
    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert(
            "role-group-test".to_owned(),
            "SYSTEM {{role.group}}/{{role.name}} {{#each prompt_fragments}}{{content}}{{/each}}"
                .to_owned(),
        );

    assert_eq!(
        h.build_system_prompt_for_role(role_name),
        "SYSTEM reviewers/security-reviewer FRAGMENT reviewers/security-reviewer"
    );
    h.shutdown().expect("shutdown");
}

/// Ensures hidden roles leave the built-in catalog but remain valid explicit
/// `agent_start` targets, preserving visibility as presentation-only metadata.
#[test]
fn hidden_delegate_roles_are_omitted_from_catalog_but_remain_explicitly_callable() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(TestAgentStartBuiltin)]);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    h.config
        .available_roles
        .get_mut("engineer-junior")
        .expect("built-in junior role")
        .visible = Some(false);
    h.publish_delegate_roles_context();

    let role = h.config.selected_role.clone();
    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        &role,
    );
    let tools = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    let rendered = h
        .try_build_system_prompt_for_role_and_agent(
            &role,
            Some(&agent_id),
            Some(&agent_id),
            &tools,
            model.as_ref(),
            false,
        )
        .expect("render prompt");
    assert!(rendered.contains("* `engineer` -"));
    assert!(!rendered.contains("* `engineer-junior` -"));

    let query = tau_proto::StartAgentRequest {
        trusted_internal_spans: Vec::new(),
        parent_agent: None,
        query_id: "hidden-role".to_owned(),
        instruction: "side task".to_owned(),
        role: Some("engineer-junior".to_owned()),
        input_stats: tau_proto::ToolUseStats::default(),
        tool_call_id: Some("hidden-role-call".into()),
        task_name: None,
    };
    assert_eq!(
        h.resolve_start_agent_request_role(&query),
        Ok("engineer-junior".to_owned())
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
        h.config.selected_model = Some("test/model".into());
        let parent = ensure_test_user_agent(&mut h);
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert("delegate-call".into(), parent);
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            StartAgentRequest {
                trusted_internal_spans: Vec::new(),
                parent_agent: None,
                query_id: "delegate-9".to_owned(),
                instruction: "side task".to_owned(),
                role: Some("engineer-senior".to_owned()),
                input_stats: tau_proto::ToolUseStats::default(),
                tool_call_id: Some("delegate-call".into()),
                task_name: None,
            },
        )
        .expect("start delegate");
        let cid = ext_query_cid(&h, "delegate-9").expect("delegated conversation");
        let agent_id = h
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conversation| conversation.identity.agent_id.clone())
            .expect("delegated agent id");
        h.agent_runtime.agent_registry.navigation_modes.insert(
            crate::parse_agent_id(&agent_id),
            tau_proto::AgentNavigationMode::Suspended,
        );
        h.shutdown().expect("shutdown");
        agent_id
    };

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    h.config.selected_role = "engineer-junior".to_owned();
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(&agent_id)
        .cloned()
        .expect("resumed delegated conversation");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conversation| conversation.identity.role.as_deref()),
        Some("engineer-senior")
    );
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .navigation_modes
            .get(&tau_proto::AgentId::parse(&agent_id).expect("agent id")),
        Some(&tau_proto::AgentNavigationMode::ActiveAuto)
    );
    h.shutdown().expect("shutdown");
}
