//! Opt-in role tool requirements protect specialized agents without changing
//! extension failure policy or the lifetime of already-running agents.

use super::*;

/// Register a deterministic tool under a different internal and public name,
/// exercising canonical declarations without starting a real Zulip process.
fn register_bot_tool(h: &mut Harness) {
    connect_ready_configured_extension(h, "bot-tools", "bot-tools", tau_proto::ClientKind::Tool);
    h.handle_extension_event_inner(
        &crate::test_connection_id("bot-tools"),
        Event::ToolRegistrationDeclared(tau_proto::ToolRegistrationDeclared {
            tool: ToolSpec {
                provider_scope: None,
                name: ToolName::new("bot_register_internal"),
                model_visible_name: Some(ToolName::new("zulip_register")),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        }),
    )
    .expect("declare bot tool");
}

/// Write a role requirement through the public configuration schema.
fn write_bot_config(state: &Path, selected: bool) {
    std::fs::create_dir_all(state.join("config")).expect("config directory");
    let selection = if selected { "default_role: bot" } else { "" };
    std::fs::write(
        state.join("config/harness.yaml"),
        format!(
            "agents:\n  {selection}\n  role_groups:\n    bots:\n      roles:\n        bot:\n          required_tools: [zulip_register]\n"
        ),
    )
    .expect("bot config");
}

/// Construct a UI request with no prompt, avoiding any provider inference.
fn bot_ui_request(model_override: Option<tau_proto::ModelId>) -> tau_proto::UiCreateAgent {
    tau_proto::UiCreateAgent {
        request_id: "bot-ui".to_owned(),
        literal: false,
        parent_agent: None,
        session_id: "s1".parse().expect("session"),
        role: "bot".to_owned(),
        model_override,
        effort_override: None,
        metadata: Vec::new(),
        initial_prompt: None,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        ephemeral: false,
    }
}

/// Snapshot canonical journals recursively so rejected resume cannot append
/// bootstrap replacements, repair facts, or new agent/session records.
fn journal_bytes(root: &Path) -> path_std_collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut path_std_collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                visit(root, &path, out);
            } else if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("events.cbor" | "restore-events.cbor")
            ) {
                out.insert(
                    path.strip_prefix(root).expect("relative path").to_owned(),
                    std::fs::read(path).expect("journal bytes"),
                );
            }
        }
    }
    let mut out = path_std_collections::BTreeMap::new();
    visit(root, root, &mut out);
    out
}

/// A selected specialized role must fail construction before session discovery
/// or inference if its tool extension did not register the required surface.
#[test]
fn required_tools_missing_selected_role_fails_fresh_startup() {
    let tmp = TempDir::new().expect("tempdir");
    write_bot_config(tmp.path(), true);
    let error = echo_harness(tmp.path()).err().expect("startup rejection");
    let message = error.to_string();
    assert!(message.contains("role `bot` cannot start"), "{message}");
    assert!(message.contains("`zulip_register`"), "{message}");
    assert!(message.contains("required_tools"), "{message}");
    assert!(
        !journal_bytes(tmp.path())
            .values()
            .any(|bytes| !bytes.is_empty())
    );
}

/// Public-name matching accepts an activated tool at the startup barrier while
/// rejecting its private backing name and requirements filtered out by policy.
#[test]
fn required_tools_startup_uses_effective_public_surface() {
    let tmp = TempDir::new().expect("tempdir");
    write_bot_config(tmp.path(), true);
    let mut h = echo_harness_with_start_reason_before_session_init(
        "s1",
        tmp.path(),
        tau_proto::SessionStartReason::Initial,
        Box::new(register_bot_tool),
    )
    .expect("registered tool allows startup");
    h.validate_required_tools_for_role("bot")
        .expect("available");
    h.config
        .available_roles
        .get_mut("bot")
        .expect("role")
        .disable_tools = vec![ToolName::new("zulip_register")];
    let error = h
        .validate_required_tools_for_role("bot")
        .expect_err("filtered out");
    assert!(error.contains("zulip_register"));
    h.config
        .available_roles
        .get_mut("bot")
        .expect("role")
        .disable_tools
        .clear();
    h.config
        .available_roles
        .get_mut("bot")
        .expect("role")
        .required_tools = vec![ToolName::new("bot_register_internal")];
    assert!(h.validate_required_tools_for_role("bot").is_err());
}

/// A registered tool unsupported by the role model is unavailable even though
/// its extension is healthy; declaration presence is not a sufficient guard.
#[test]
fn required_tools_enforce_model_tool_support() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    register_bot_tool(&mut h);
    h.config
        .available_roles
        .get_mut("engineer")
        .expect("role")
        .required_tools = vec![ToolName::new("zulip_register")];
    h.validate_required_tools_for_role("engineer")
        .expect("supported");
    for info in h.provider_runtime.model_info.values_mut() {
        info.supported_tool_types.clear();
    }
    assert!(h.validate_required_tools_for_role("engineer").is_err());
}

/// Resume checks restored role requirements even when the selected default role
/// has none, and rejects before altering either session or agent journals.
#[test]
fn required_tools_missing_restored_role_rejects_resume_without_journal_mutation() {
    let tmp = TempDir::new().expect("tempdir");
    {
        let mut h = echo_harness(tmp.path()).expect("initial harness");
        let mut role = h.config.available_roles["engineer"].clone();
        role.required_tools.clear();
        h.config.available_roles.insert("bot".to_owned(), role);
        h.create_durable_user_agent("s1".parse().expect("session"), "bot");
        h.shutdown().expect("shutdown");
    }
    write_bot_config(tmp.path(), false);
    let before = journal_bytes(tmp.path());
    assert!(!before.is_empty());
    let error =
        echo_harness_with_start_reason("s1", tmp.path(), tau_proto::SessionStartReason::Resume)
            .err()
            .expect("resume rejection");
    assert!(
        error.to_string().contains("role `bot` cannot start"),
        "{error}"
    );
    assert_eq!(journal_bytes(tmp.path()), before);
}

/// An interrupted compaction requires a durable replay repair. Missing role
/// tools must reject before that repair changes any journal; a later healthy
/// resume must still publish the existing interrupted outcome exactly once.
#[test]
fn required_tools_rejected_resume_preserves_interrupted_compaction_until_healthy_resume() {
    let tmp = TempDir::new().expect("tempdir");
    let transaction_id = tau_proto::CompactionTransactionId::parse("required-tools-interrupted")
        .expect("transaction");
    let agent_id = {
        let mut h = echo_harness(tmp.path()).expect("initial harness");
        let mut role = h.config.available_roles["engineer"].clone();
        role.required_tools.clear();
        h.config.available_roles.insert("bot".to_owned(), role);
        let cid = h.create_durable_user_agent("s1".parse().expect("session"), "bot");
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        let cut = h
            .selected_head_for_agent(&cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        h.append_direct_agent_semantic_event(
            agent_id.as_str(),
            tau_core::AgentEventParent::from_head(cut),
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: tau_proto::AgentPromptId::parse("required-tools-compact")
                    .expect("compact prompt"),
                cut,
                resume_through: None,
                model: "echo/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
        )
        .expect("seed interrupted compaction");
        h.shutdown().expect("shutdown");
        agent_id
    };
    write_bot_config(tmp.path(), false);
    let before = journal_bytes(tmp.path());
    let error =
        echo_harness_with_start_reason("s1", tmp.path(), tau_proto::SessionStartReason::Resume)
            .err()
            .expect("missing tool rejects resume");
    assert!(error.to_string().contains("zulip_register"), "{error}");
    assert_eq!(journal_bytes(tmp.path()), before);
    let mut h = echo_harness_with_start_reason_before_session_init(
        "s1",
        tmp.path(),
        tau_proto::SessionStartReason::Resume,
        Box::new(register_bot_tool),
    )
    .expect("healthy resume");
    let repairs = h.session_runtime.agent_store.snapshot_agent_events_for_test(agent_id.as_str())
        .expect("agent journal").into_iter().filter(|record| matches!(
            &record.event, Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == transaction_id
                    && failed.reason == tau_proto::StandaloneCompactionFailureReason::Interrupted
        )).count();
    assert_eq!(repairs, 1);
    h.shutdown().expect("shutdown");
}

/// A tool disconnect must reject subsequent side-agent and UI/direct creation
/// without accepting a new start, killing an existing bot, or blocking
/// unrelated roles. Restoring the tool permits new creation without restarting
/// the harness.
#[test]
fn required_tools_disconnect_blocks_new_agents_not_running_agents() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    register_bot_tool(&mut h);
    let mut role = h.config.available_roles["engineer"].clone();
    role.required_tools = vec![ToolName::new("zulip_register")];
    h.config.available_roles.insert("bot".to_owned(), role);
    let existing = h.create_durable_user_agent("s1".parse().expect("session"), "bot");
    h.handle_disconnect(&crate::test_connection_id("bot-tools"));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&existing)
    );
    let persistence = h
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("persistence owner");
    // Finish setup writes before comparing physical journals across rejections.
    assert_eq!(
        persistence.wait_for_latest_durability_for_test(Duration::from_secs(5)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    let before = journal_bytes(tmp.path());
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    h.handle_start_agent_request(
        &crate::test_connection_id("requester"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            query_id: "bot-missing".to_owned(),
            instruction: "must not run".to_owned(),
            role: Some("bot".to_owned()),
            input_stats: Default::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: None,
        },
    )
    .expect("local rejection");
    let events = sink.lock().expect("sink");
    assert!(!events.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame), Some(Event::StartAgentAccepted(accepted)) if accepted.query_id == "bot-missing"
    )));
    assert!(events.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame), Some(Event::StartAgentResult(result))
            if result.query_id == "bot-missing" && result.error.as_deref().is_some_and(|error| error.contains("zulip_register"))
    )));
    drop(events);
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("requester"),
        bot_ui_request(None),
    )
    .expect("UI rejection");
    assert!(sink.lock().expect("sink").iter().any(|frame| matches!(
        peel_inner_event(&frame.frame), Some(Event::UiCreateAgentResult(result))
            if matches!(&result.outcome, tau_proto::UiCreateAgentOutcome::Rejected {
                reason: tau_proto::UiCreateAgentRejection::RoleUnavailable, message, ..
            } if message.contains("zulip_register"))
    )));
    let error = h
        .try_create_durable_user_agent("s1".parse().expect("session"), "bot")
        .expect_err("direct creation rejected");
    assert!(error.to_string().contains("required_tools"));
    // Include any mistakenly admitted asynchronous writes in the comparison.
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("persistence owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(5)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    assert_eq!(journal_bytes(tmp.path()), before);
    h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
    register_bot_tool(&mut h);
    h.create_durable_user_agent("s1".parse().expect("session"), "bot");
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&existing)
    );
}

/// A request queued while tools are healthy must recheck when it starts after a
/// disconnect, rather than accepting a stale intake-time availability decision.
#[test]
fn required_tools_queued_start_rechecks_before_acceptance() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    register_bot_tool(&mut h);
    h.config
        .available_roles
        .get_mut("engineer")
        .expect("role")
        .required_tools = vec![ToolName::new("zulip_register")];
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let pending = h
        .prepare_start_agent_request(
            &crate::test_connection_id("requester"),
            StartAgentRequest {
                trusted_internal_spans: Vec::new(),
                query_id: "queued-bot".to_owned(),
                instruction: "must not run".to_owned(),
                role: Some("engineer".to_owned()),
                input_stats: Default::default(),
                tool_call_id: None,
                task_name: None,
                parent_agent: None,
            },
        )
        .expect("intake")
        .expect("new request");
    h.handle_disconnect(&crate::test_connection_id("bot-tools"));
    h.begin_start_operation(pending);
    let frames = sink.lock().expect("sink");
    assert!(!frames.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame),
        Some(Event::StartAgentAccepted(_))
    )));
    assert!(frames.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame), Some(Event::StartAgentResult(result))
            if result.error.as_deref().is_some_and(|error| error.contains("zulip_register"))
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .is_empty()
    );
}

/// UI admission must use an available model override, not falsely reject a tool
/// supported by that model just because the role's default model cannot use it.
#[test]
fn required_tools_ui_creation_uses_effective_model_override() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    register_bot_tool(&mut h);
    let mut role = h.config.available_roles["engineer"].clone();
    role.required_tools = vec![ToolName::new("zulip_register")];
    h.config.available_roles.insert("bot".to_owned(), role);
    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        "bot",
    )
    .expect("default model");
    let alternate: tau_proto::ModelId = format!("{}/alternate", model.provider).into();
    let mut info = h.provider_runtime.model_info[&model].clone();
    info.id = alternate.clone();
    h.provider_runtime
        .model_info
        .insert(alternate.clone(), info);
    h.provider_runtime.model_routes.insert(
        alternate.clone(),
        h.provider_runtime.model_routes[&model].clone(),
    );
    h.provider_runtime
        .model_info
        .get_mut(&model)
        .expect("default")
        .supported_tool_types
        .clear();
    // Force the default rather than letting affinity select the new model.
    h.config.available_roles.get_mut("bot").expect("role").model = Some(model);
    assert!(h.validate_required_tools_for_role("bot").is_err());
    let sink = connect_test_tool(&mut h, "ui-override");
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-override"),
        bot_ui_request(Some(alternate.clone())),
    )
    .expect("create with supported override");
    assert!(sink.lock().expect("sink").iter().any(|frame| matches!(
        peel_inner_event(&frame.frame), Some(Event::UiCreateAgentResult(result))
            if matches!(result.outcome, tau_proto::UiCreateAgentOutcome::Created { .. })
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .any(|agent| agent.identity.model_override.as_ref() == Some(&alternate))
    );
}

/// A non-selected role's missing tools must not prevent an ordinary default
/// session from starting; requirements are not global extension health policy.
#[test]
fn required_tools_unselected_role_does_not_block_unrelated_startup() {
    let tmp = TempDir::new().expect("tempdir");
    write_bot_config(tmp.path(), false);
    let mut h = echo_harness(tmp.path()).expect("unrelated role starts");
    assert!(
        h.config.available_roles["engineer"]
            .required_tools
            .is_empty()
    );
    assert!(h.validate_required_tools_for_role("bot").is_err());
    h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
    assert!(
        h.required_tool_roles_before_restore()
            .expect("read-only role inspection")
            .is_empty(),
        "an unused required role must not defer restoration of unrelated agents"
    );
}
