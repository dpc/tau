//! Tests for rendered previews behavior.

use super::*;

fn register_managed_fetch_backings(harness: &mut Harness) {
    let connection_id = crate::test_connection_id("managed-fetch-backings");
    for (name, enabled_by_default) in [
        ("websearch_hybrid_fetch", true),
        ("websearch_exa_fetch", false),
        ("websearch_parallel_fetch", false),
    ] {
        harness.tool_routing.registry.register(
            &connection_id,
            tau_proto::ToolSpec {
                provider_scope: None,
                name: tau_proto::ToolName::new(name),
                model_visible_name: Some(tau_proto::ToolName::new("web_fetch")),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({"type": "object"})),
                format: None,
                tags: vec![
                    tau_proto::ToolTag::new(tau_proto::TURN_DATA_FETCH_TOOL_TAG),
                    tau_proto::ToolTag::new(tau_proto::WEB_FETCH_TOOL_TAG),
                    tau_proto::ToolTag::new(tau_proto::WEB_REQUESTED_TARGET_DOMAIN_ENFORCEMENT_TAG),
                ],
                enabled_by_default,
                background_support: None,
                examples: Vec::new(),
            },
        );
    }
}

fn enable_public_fetch_name(harness: &mut Harness, role: &str) {
    harness
        .config
        .available_roles
        .get_mut(role)
        .expect("selected role")
        .enable_tools
        .push(tau_proto::ToolName::new("web_fetch"));
}

/// Cancelling a disconnected preview requester must discard its waiting
/// ephemeral agent so a context provider that never becomes ready cannot leak
/// runtime routes or deferred response state.
#[test]
fn disconnect_cancels_pending_rendered_preview() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let requester = crate::test_connection_id("preview-disconnect-requester");
    let _frames = connect_test_client(&mut h, requester.as_str(), tau_proto::ClientKind::Ui);
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.prompt_coordination
        .context_discovery
        .pending_rendered_prompts
        .insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![PendingRenderedPrompt::Prompt {
                    connection_id: requester.clone(),
                    request_id: "preview-disconnect".to_owned(),
                    role: h.config.selected_role.clone(),
                    enable_agents_md: false,
                }],
                deadline: Instant::now(),
            },
        );

    h.handle_disconnect_at(&requester, Instant::now());

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_rendered_prompts
            .contains_key(&agent_id)
    );
    assert!(
        h.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
    );
}

/// An extension that never completes per-agent context must hit the bounded
/// preview deadline and release both request state and the temporary agent.
#[test]
fn rendered_preview_context_timeout_cleans_up_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let deadline = Instant::now();
    h.prompt_coordination
        .context_discovery
        .pending_rendered_prompts
        .insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![PendingRenderedPrompt::Tools {
                    connection_id: crate::test_connection_id("preview-timeout-requester"),
                    request_id: "preview-timeout".to_owned(),
                    role: h.config.selected_role.clone(),
                }],
                deadline,
            },
        );

    h.process_rendered_preview_deadlines(deadline);

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_rendered_prompts
            .contains_key(&agent_id)
    );
    assert!(
        h.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
    );
}

/// Developer tool previews must expose the same post-policy ordinary/hosted
/// surface as live dispatch for native replacement, ordinary fallback, and
/// unavailable exact-route metadata.
#[test]
fn rendered_tool_preview_matches_live_web_tool_materialization() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let role = h.config.selected_role.clone();
    let model = tau_proto::ModelId::from("test/model");
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let connection_id = crate::test_connection_id("web-preview-tools");
    for (name, alias, tags) in [
        (
            "websearch_hybrid_search",
            "web_search",
            vec![tau_proto::WEB_SEARCH_TOOL_TAG],
        ),
        (
            "websearch_hybrid_fetch",
            "web_fetch",
            vec![
                tau_proto::WEB_FETCH_TOOL_TAG,
                tau_proto::WEB_REQUESTED_TARGET_DOMAIN_ENFORCEMENT_TAG,
            ],
        ),
    ] {
        h.tool_routing.registry.register(
            &connection_id,
            tau_proto::ToolSpec {
                provider_scope: None,
                name: tau_proto::ToolName::new(name),
                model_visible_name: Some(tau_proto::ToolName::new(alias)),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({"type": "object"})),
                format: None,
                tags: tags.into_iter().map(tau_proto::ToolTag::new).collect(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
    }

    let model_info = h
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("quiet provider model metadata");
    model_info.hosted_tool_capabilities =
        vec![tau_proto::ProviderHostedToolCapability::WebSearch {
            access_modes: vec![tau_proto::ProviderWebSearchAccess::Cached],
            supports_allowed_domains: true,
            supports_context_size: true,
        }];

    let native_preview = h
        .prepare_prompt_surface_for_preview(&role, &agent_id, &model)
        .expect("native preview")
        .into_tool_surface();
    let native_live = h
        .prepare_tool_surface_for_dispatch(&role, &agent_id, &model)
        .expect("native live surface");
    assert_eq!(native_preview, native_live);
    assert!(matches!(
        native_preview.1.as_slice(),
        [tau_proto::HostedToolDefinition::WebSearch {
            access: tau_proto::ProviderWebSearchAccess::Cached,
            ..
        }]
    ));
    assert!(
        native_preview
            .0
            .iter()
            .all(|tool| tool.name.as_str() != "websearch_hybrid_search")
    );
    assert!(
        native_preview
            .0
            .iter()
            .any(|tool| tool.name.as_str() == "websearch_hybrid_fetch")
    );

    h.provider_runtime
        .model_info
        .get_mut(&model)
        .expect("quiet provider model metadata")
        .hosted_tool_capabilities
        .clear();
    let fallback_preview = h
        .prepare_prompt_surface_for_preview(&role, &agent_id, &model)
        .expect("ordinary fallback preview")
        .into_tool_surface();
    let fallback_live = h
        .prepare_tool_surface_for_dispatch(&role, &agent_id, &model)
        .expect("ordinary fallback live surface");
    assert_eq!(fallback_preview, fallback_live);
    assert!(fallback_preview.1.is_empty());
    assert!(
        fallback_preview
            .0
            .iter()
            .any(|tool| tool.name.as_str() == "websearch_hybrid_search")
    );

    h.provider_runtime.model_info.remove(&model);
    assert!(matches!(
        h.prepare_prompt_surface_for_preview(&role, &agent_id, &model),
        Err(PromptSurfaceError::WebUnavailable(message))
            if message.contains("model capability metadata is unavailable")
    ));
}

/// Public-name enablement makes every managed fetch backing eligible, while
/// logical-web policy still exposes only its configured ordinary winner.
#[test]
fn exact_web_materialization_projects_managed_fetch_family_before_collision_check() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let role = h.config.selected_role.clone();
    let model = h
        .config
        .selected_model
        .clone()
        .expect("quiet provider selected model");
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    register_managed_fetch_backings(&mut h);
    enable_public_fetch_name(&mut h, &role);

    let (tools, hosted) = h
        .prepare_tool_surface_for_dispatch(&role, &agent_id, &model)
        .expect("configured fetch winner");
    assert!(hosted.is_empty());
    let fetches = tools
        .iter()
        .filter(|tool| tool.name.as_str() == "websearch_hybrid_fetch")
        .count();
    assert_eq!(fetches, 1);
    assert!(
        tools.iter().all(|tool| {
            !matches!(
                tool.name.as_str(),
                "websearch_exa_fetch" | "websearch_parallel_fetch"
            )
        }),
        "{tools:#?}"
    );

    h.tool_routing.registry.register(
        &crate::test_connection_id("unrelated-fetch-alias"),
        tau_proto::ToolSpec {
            provider_scope: None,
            name: tau_proto::ToolName::new("unrelated_fetch"),
            model_visible_name: Some(tau_proto::ToolName::new("web_fetch")),
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({"type": "object"})),
            format: None,
            tags: Vec::new(),
            enabled_by_default: false,
            background_support: None,
            examples: Vec::new(),
        },
    );
    assert!(matches!(
        h.prepare_tool_surface_for_dispatch(&role, &agent_id, &model),
        Err(PromptSurfaceError::DuplicateToolName(name)) if name == "web_fetch"
    ));
}

/// Metadata-less developer introspection selects the configured ordinary fetch
/// implementation and retains its existing provisional-resolution warning.
#[test]
fn rendered_tool_preview_without_model_metadata_projects_managed_fetch_family() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let role = h.config.selected_role.clone();
    register_managed_fetch_backings(&mut h);
    enable_public_fetch_name(&mut h, &role);
    h.provider_runtime.model_info.clear();
    let requester = crate::test_connection_id("provisional-web-preview");
    let frames = connect_test_client(&mut h, requester.as_str(), tau_proto::ClientKind::Ui);

    h.send_rendered_tool_definitions_result(
        &requester,
        tau_proto::GetRenderedToolDefinitions {
            request_id: "provisional-web-preview".to_owned(),
            role: Some(role.clone()),
        },
    );

    let frames = frames.lock().expect("preview frames");
    let result = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::RenderedToolDefinitionsResult(result)
                if result.request_id == "provisional-web-preview" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("rendered tool definitions");
    assert_eq!(result.error, None);
    assert!(result.hosted_tools.is_empty());
    assert_eq!(
        result.warnings,
        vec![format!(
            "role `{role}` has no exact model capability metadata; provider-native replacement could not be resolved, so ordinary tool entries are provisional"
        )]
    );
    let tools = result.tools.as_ref().expect("provisional ordinary tools");
    assert!(
        tools
            .iter()
            .any(|tool| tool.name.as_str() == "websearch_hybrid_fetch")
    );
    assert!(tools.iter().all(|tool| {
        !matches!(
            tool.name.as_str(),
            "websearch_exa_fetch" | "websearch_parallel_fetch"
        )
    }));
}
