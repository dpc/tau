//! Main-only capability tests exercise identity rather than role or parent
//! shape.

use tau_proto::{
    BridgeReceiverMode as Mode, BridgeReceiverOutcome as Outcome, BridgeReceiverRequest,
    BridgeReceiverTarget, BridgeReceiverUnavailable,
};

use super::*;

/// Resolve a main-constrained bridge receiver through authenticated admission.
fn resolve(h: &mut Harness, role: Option<&str>, mode: Mode) -> Outcome {
    h.resolve_bridge_receiver(
        &crate::test_connection_id("bridge"),
        &BridgeReceiverRequest {
            request_id: "main-receiver".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: role.map(str::to_owned),
            target: BridgeReceiverTarget::CanonicalMain,
            mode,
        },
        &h.current_extension_frame_admission(),
    )
}

/// Register an aliased, explicitly re-enabled tool to challenge the final veto.
fn register_restricted_tool(h: &mut Harness) -> ToolSpec {
    let spec = register_aliased_tool(h);
    h.config.tool_policy.canonical_main_only = vec![spec.name.clone()];
    spec
}

/// Provision the aliased tool without changing the configured identity policy.
fn register_aliased_tool(h: &mut Harness) -> ToolSpec {
    connect_ready_message_publisher(h, "bridge", "test-bridge");
    let mut spec = super::dispatch::shared_test_tool_spec("bot_zulip_send");
    spec.model_visible_name = Some(ToolName::new("bot_dm"));
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("bridge"), spec.clone());
    h.config
        .available_roles
        .get_mut("engineer")
        .expect("role")
        .enable_tools = vec![spec.name.clone()];
    spec
}

/// Profile-loaded restrictions reach real harness construction and both final
/// vetoes without granting access to a same-role worker or stale prompt.
#[test]
fn canonical_main_profile_policy_reaches_visibility_and_dispatch() {
    let td = TempDir::new().expect("tempdir");
    std::fs::create_dir(td.path().join("config")).expect("config directory");
    std::fs::write(
        td.path().join("config/harness.yaml"),
        r#"
default_profile: manager
profiles:
  manager:
    tool_policy:
      canonical_main_only: [bot_zulip_send]
"#,
    )
    .expect("profile fixture");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let spec = register_aliased_tool(&mut h);
    let worker = h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    assert_tool_access(&mut h, &spec, &worker, false);
    assert_tool_access(&mut h, &spec, &main, true);
    h.shutdown().expect("shutdown");
}

/// Exercise visibility and actual dispatch, including a stale privileged
/// snapshot.
fn assert_tool_access(h: &mut Harness, spec: &ToolSpec, id: &AgentId, allowed: bool) {
    let model = h.config.selected_model.clone().expect("model");
    let (tools, _) = h
        .prepare_tool_surface_for_dispatch("engineer", id, &model)
        .expect("surface");
    assert_eq!(tools.iter().any(|tool| tool.name == spec.name), allowed);
    let call_id = ToolCallId::new(format!("main-only-{id}-{allowed}"));
    let prompt = tau_proto::AgentPromptId::parse(format!("stale-{id}")).expect("prompt id");
    h.prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(prompt.clone(), vec![spec.clone()]);
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(call_id.clone(), prompt);
    h.execute_agent_tool_call(
        id,
        &AgentToolCall {
            call_ref: None,
            id: call_id.clone(),
            name: ToolName::new("bot_dm"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
    )
    .expect("dispatch");
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key(&call_id),
        allowed,
    );
    assert_eq!(
        super::dispatch::event_log_count(h, |event| matches!(
            event, Event::ProviderToolError(error) if error.call_id == call_id
                && error.message.contains("restricted to the available canonical session main")
        )) > 0,
        !allowed
    );
}

/// Same-role workers, tool children, serve bootstrap and pending candidates
/// cannot inherit main capabilities; retained main alone keeps access after
/// cold replay.
#[test]
fn canonical_main_tools_reject_other_identities_and_survive_cold_replay() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (main, ordinary, child, bootstrap) = {
        let mut h = quiet_provider_harness(&state).expect("harness");
        configure_main_role(&mut h, "engineer");
        let spec = register_restricted_tool(&mut h);
        let ordinary = h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
        h.session_runtime.current_session_state.main_candidate = Some(ordinary.clone());
        assert!(h.is_main_agent_load(&ordinary));
        assert!(h.main_agent_id().is_none());
        assert_tool_access(&mut h, &spec, &ordinary, false);
        assert_eq!(
            resolve(
                &mut h,
                None,
                Mode::Ensure {
                    preferred_agent_id: None
                }
            ),
            Outcome::Unavailable {
                reason: BridgeReceiverUnavailable::NotReady
            }
        );
        assert!(h.main_agent_id().is_none());
        h.session_runtime.current_session_state.main_candidate = None;

        let call = ToolCallId::new("child-start");
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call.clone(), ordinary.clone());
        let pending = h
            .prepare_start_agent_request(
                crate::harness::harness_connection_id(),
                tau_proto::StartAgentRequest {
                    query_id: "child".to_owned(),
                    instruction: String::new(),
                    role: Some("engineer".to_owned()),
                    tool_call_id: Some(call),
                    trusted_internal_spans: vec![],
                    input_stats: Default::default(),
                    task_name: None,
                    parent_agent: Some(ordinary.clone()),
                },
            )
            .expect("prepare")
            .expect("child");
        let child = pending.cid.clone();
        h.start_agent_request_inner(pending, false, None, false, None)
            .expect("start");

        let _ui = connect_test_client(&mut h, "bootstrap-ui", tau_proto::ClientKind::Ui);
        h.register_bootstrap_create("bootstrap".to_owned(), "bot-bootstrap".to_owned());
        h.handle_ui_create_agent_from(
            &crate::test_connection_id("bootstrap-ui"),
            tau_proto::UiCreateAgent {
                request_id: "bootstrap".to_owned(),
                session_id: h.session_runtime.current_session_id.clone(),
                role: "engineer".to_owned(),
                model_override: None,
                effort_override: None,
                metadata: Vec::new(),
                initial_prompt: None,
                literal: true,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
                parent_agent: None,
                ephemeral: false,
            },
        )
        .expect("bootstrap");
        let bootstrap = h
            .bootstrap_agent_for_id("bot-bootstrap")
            .expect("scan")
            .expect("id");
        let main = h.ensure_main_agent().expect("ensure").expect("main");
        for id in [&ordinary, &child, &bootstrap] {
            assert_tool_access(&mut h, &spec, id, false);
            assert_eq!(
                resolve(
                    &mut h,
                    None,
                    Mode::Restore {
                        agent_id: Some(id.clone())
                    }
                ),
                Outcome::Unavailable {
                    reason: BridgeReceiverUnavailable::NoEligibleAgent
                }
            );
            let call = ToolCallId::new(format!("register-{id}"));
            h.tool_routing
                .tool_runtime
                .tool_agents
                .insert(call.clone(), id.clone());
            h.tool_routing
                .tool_runtime
                .pending_tool_providers
                .insert(call.clone(), crate::test_connection_id("bridge"));
            assert!(matches!(
                resolve(&mut h, None, Mode::Register { tool_call_id: call }),
                Outcome::Error {
                    kind: tau_proto::BridgeReceiverErrorKind::InvalidCaller,
                    ..
                }
            ));
        }
        assert_tool_access(&mut h, &spec, &main, true);
        h.shutdown().expect("shutdown");
        (main, ordinary, child, bootstrap)
    };
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let spec = register_restricted_tool(&mut h);
    assert_eq!(h.main_agent_id(), Some(&main));
    for id in [&ordinary, &child, &bootstrap] {
        // A child may no longer be loaded after shutdown; no identity
        // substitution is permitted either way.
        assert!(!h.tool_identity_is_authorized(&spec.name, Some(id)));
        assert_eq!(
            resolve(
                &mut h,
                None,
                Mode::Restore {
                    agent_id: Some(id.clone())
                }
            ),
            Outcome::Unavailable {
                reason: BridgeReceiverUnavailable::NoEligibleAgent
            }
        );
    }
    assert_tool_access(&mut h, &spec, &main, true);
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer"),
            Mode::Restore {
                agent_id: Some(main.clone())
            }
        ),
        Outcome::Selected {
            agent_id: main.clone()
        }
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&main)
        .expect("main")
        .dispatch
        .terminating = true;
    assert_tool_access(&mut h, &spec, &main, false);
    h.shutdown().expect("shutdown");
}

/// Main receiver creation remains lazy and prompt-free, preserves role
/// constraints, rejects stale manual designations, and never replaces an
/// unavailable bound main.
#[test]
fn canonical_main_receiver_lazy_ensure_and_fail_closed_selection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    let ordinary = h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
    let absent = Outcome::Unavailable {
        reason: BridgeReceiverUnavailable::NoEligibleAgent,
    };
    assert_eq!(
        resolve(
            &mut h,
            None,
            Mode::Ensure {
                preferred_agent_id: None
            }
        ),
        absent
    );
    configure_main_role(&mut h, "engineer");
    assert_eq!(
        resolve(
            &mut h,
            None,
            Mode::Select {
                preferred_agent_id: Some(ordinary.clone())
            }
        ),
        absent
    );
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer-junior"),
            Mode::Ensure {
                preferred_agent_id: None
            }
        ),
        absent
    );
    assert!(h.main_agent_id().is_none());
    let Outcome::Selected { agent_id: main } = resolve(
        &mut h,
        None,
        Mode::Ensure {
            preferred_agent_id: Some(ordinary.clone()),
        },
    ) else {
        panic!("main creation");
    };
    assert_eq!(h.main_agent_id(), Some(&main));
    assert_ne!(main, ordinary);
    assert_eq!(
        resolve(
            &mut h,
            None,
            Mode::Restore {
                agent_id: Some(ordinary.clone())
            }
        ),
        absent
    );
    assert_eq!(
        resolve(
            &mut h,
            None,
            Mode::Select {
                preferred_agent_id: Some(ordinary)
            }
        ),
        Outcome::Selected {
            agent_id: main.clone()
        }
    );
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer-junior"),
            Mode::Select {
                preferred_agent_id: None
            }
        ),
        absent
    );
    let call = ToolCallId::new("register-main");
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call.clone(), main.clone());
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call.clone(), crate::test_connection_id("bridge"));
    assert_eq!(
        resolve(&mut h, None, Mode::Register { tool_call_id: call }),
        Outcome::Selected {
            agent_id: main.clone()
        }
    );
    let events = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(main.as_str())
        .expect("events");
    assert!(events.iter().all(|record| !matches!(
        record.event,
        Event::AgentPromptSubmitted(_) | Event::AgentPromptStarted(_)
    )));
    h.handle_extension_event_inner(
        &crate::test_connection_id("bridge"),
        Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
            tau_proto::RawMessagePublisherId::new("test-bridge"),
            tau_proto::MessageAgentTarget::new(main.as_str()),
            tau_proto::MessageFactId::new("main-dm"),
            tau_proto::MessageParty {
                stable_id: "allowed-user".to_owned(),
                display_name: None,
                sender_auth: None,
                sender_trust: None,
            },
            None,
            "admitted DM",
        )),
    )
    .expect("deliver to selected main");
    assert_eq!(
        super::dispatch::event_log_count(&h, |event| matches!(
            event, Event::MessageDelivered(message)
                if message.agent_id.as_str() == main.as_str() && message.message_id.as_str() == "main-dm"
        )),
        1
    );
    let count = h.agent_runtime.agent_registry.agents.len();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&main)
        .expect("main")
        .dispatch
        .terminating = true;
    for mode in [
        Mode::Restore {
            agent_id: Some(main.clone()),
        },
        Mode::Select {
            preferred_agent_id: None,
        },
        Mode::Ensure {
            preferred_agent_id: None,
        },
    ] {
        assert_eq!(resolve(&mut h, None, mode), absent);
    }
    assert_eq!(h.main_agent_id(), Some(&main));
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), count);
    h.shutdown().expect("shutdown");
}

/// Required-tools declarations validate the final instance surface, not a grant
/// inherited from a shared role; unscoped tools retain ordinary availability.
#[test]
fn canonical_main_required_tools_cannot_restore_withheld_authority() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let spec = register_restricted_tool(&mut h);
    let ordinary = h.create_durable_user_agent("s1".parse().expect("session"), "engineer");
    h.config
        .available_roles
        .get_mut("engineer")
        .expect("role")
        .required_tools = vec![ToolName::new("bot_dm")];
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    let model = h.config.selected_model.clone().expect("model");
    assert!(h.validate_required_tools_for_role("engineer").is_ok());
    assert!(matches!(
        h.prepare_tool_surface_for_dispatch("engineer", &ordinary, &model),
        Err(super::super::prompt_materialization::PromptSurfaceError::RequiredToolsUnavailable(message))
            if message.contains("bot_dm")
    ));
    assert!(
        h.prepare_tool_surface_for_dispatch("engineer", &main, &model)
            .is_ok()
    );
    assert!(!h.tool_identity_is_authorized(&spec.name, None));
    assert!(h.tool_identity_is_authorized(&ToolName::new("other_zulip_send"), Some(&ordinary)));
    h.shutdown().expect("shutdown");
}
