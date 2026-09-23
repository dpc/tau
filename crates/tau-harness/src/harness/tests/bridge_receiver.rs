use tau_proto::{
    BridgeReceiverErrorKind as ErrorKind, BridgeReceiverMode as Mode,
    BridgeReceiverOutcome as Outcome, BridgeReceiverRequest,
    BridgeReceiverUnavailable as Unavailable,
};

use super::*;

/// Validate fixed fixture session identifiers.
fn test_session_id(id: &str) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(id).expect("session")
}

/// Build one current-session request from the configured test bridge.
fn resolve(h: &mut Harness, role: Option<&str>, mode: Mode) -> Outcome {
    h.resolve_bridge_receiver(
        &crate::test_connection_id("bridge"),
        &BridgeReceiverRequest {
            request_id: "resolve-1".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: role.map(str::to_owned),
            mode,
        },
        &h.current_extension_frame_admission(),
    )
}

/// Extract a concrete result while retaining full diagnostics on failure.
fn selected(outcome: Outcome) -> AgentId {
    match outcome {
        Outcome::Selected { agent_id } => agent_id,
        other => panic!("expected selected receiver, got {other:?}"),
    }
}

/// Selection and startup resolution remain inert until explicit admitted-input
/// ensure; no-role ensure never guesses a role and a saved busy agent stays
/// sticky.
#[test]
fn bridge_receiver_opt_in_lazy_creation_and_sticky_selection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    let absent = Outcome::Unavailable {
        reason: Unavailable::NoEligibleAgent,
    };
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer"),
            Mode::Select {
                preferred_agent_id: None
            }
        ),
        absent
    );
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
    assert_eq!(
        resolve(&mut h, Some("engineer"), Mode::Restore { agent_id: None }),
        absent
    );
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    let first = selected(resolve(
        &mut h,
        Some("engineer"),
        Mode::Ensure {
            preferred_agent_id: None,
        },
    ));
    assert!(
        h.agent_runtime.agent_registry.agents[&first]
            .identity
            .bridge_receiver_endpoint
    );
    assert!(
        !h.agent_runtime.agent_registry.agents[&first]
            .identity
            .peer_entrypoint_endpoint
    );
    assert!(!h.is_non_tool_extension_query(&first));
    let events = h
        .session_runtime
        .agent_store
        .agent_events(first.as_str())
        .expect("events");
    assert!(events.iter().all(|record| !matches!(
        record.event,
        Event::AgentPromptSubmitted(_) | Event::AgentPromptStarted(_)
    )));
    let second = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    assert_eq!(
        selected(resolve(
            &mut h,
            None,
            Mode::Select {
                preferred_agent_id: None
            }
        )),
        first
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&second)
        .expect("second")
        .turn
        .published_runtime_state = tau_proto::AgentRuntimeState::Running;
    assert_eq!(
        selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Select {
                preferred_agent_id: Some(second.clone())
            }
        )),
        second
    );
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 2);
    h.shutdown().expect("shutdown");
}

/// Saved exact identity survives cold replay, while explicit unload never
/// causes exact-ID resurrection and only automatic selection may fall back.
#[test]
fn bridge_receiver_restore_and_unload_preserve_identity_boundary() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let saved = {
        let mut h = quiet_provider_harness(&state).expect("harness");
        connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
        let id = selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Ensure {
                preferred_agent_id: None,
            },
        ));
        h.shutdown().expect("shutdown");
        id
    };
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    assert_eq!(
        selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Restore {
                agent_id: Some(saved.clone())
            }
        )),
        saved
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&saved]
            .identity
            .bridge_receiver_endpoint
    );
    assert!(!h.is_non_tool_extension_query(&saved));
    h.remove_agent_expected(&saved);
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer"),
            Mode::Restore {
                agent_id: Some(saved.clone())
            }
        ),
        Outcome::Unavailable {
            reason: Unavailable::NoEligibleAgent
        }
    );
    let replacement = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    assert_eq!(
        selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Select {
                preferred_agent_id: Some(saved.clone())
            }
        )),
        replacement
    );
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&saved));
    h.shutdown().expect("shutdown");
}

/// Registration validates exact live tool routing and immutable creation role,
/// excluding arbitrary extension side queries and never substituting a
/// receiver.
#[test]
fn bridge_receiver_register_checks_authenticated_caller_and_creation_role() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    let id = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    let call = tau_proto::ToolCallId::new("register-call");
    let mode = Mode::Register {
        tool_call_id: call.clone(),
    };
    assert!(matches!(
        resolve(&mut h, None, mode.clone()),
        Outcome::Error {
            kind: ErrorKind::InvalidCaller,
            ..
        }
    ));
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call.clone(), id.clone());
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call.clone(), crate::test_connection_id("other"));
    assert!(matches!(
        resolve(&mut h, None, mode.clone()),
        Outcome::Error {
            kind: ErrorKind::InvalidCaller,
            ..
        }
    ));
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call.clone(), crate::test_connection_id("bridge"));
    // Mutable display/runtime role is not receiving-role authority.
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&id)
        .expect("agent")
        .identity
        .role = Some("not-creation-role".to_owned());
    assert_eq!(
        selected(resolve(&mut h, Some("engineer"), mode.clone())),
        id
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&id)
        .expect("agent")
        .identity
        .originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("test-bridge"),
        query_id: "one-shot".to_owned(),
    };
    assert!(matches!(
        resolve(&mut h, None, mode.clone()),
        Outcome::Error {
            kind: ErrorKind::InvalidCaller,
            ..
        }
    ));
    h.tool_routing.tool_runtime.tool_agents.remove(&call);
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .remove(&call);
    h.shutdown().expect("shutdown");
}

/// Readiness, session, capability and role checks cannot accidentally
/// provision; ephemeral receivers retain ordinary lifecycle without persistent
/// journals.
#[test]
fn bridge_receiver_authority_readiness_and_ephemeral_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness_memory_only(td.path()).expect("memory harness");
    assert!(matches!(
        resolve(
            &mut h,
            Some("engineer"),
            Mode::Ensure {
                preferred_agent_id: None
            }
        ),
        Outcome::Error {
            kind: ErrorKind::Unauthorized,
            ..
        }
    ));
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    h.agent_runtime.agent_registry.roster_valid = false;
    assert_eq!(
        resolve(
            &mut h,
            Some("engineer"),
            Mode::Ensure {
                preferred_agent_id: None
            }
        ),
        Outcome::Unavailable {
            reason: Unavailable::NotReady
        }
    );
    h.agent_runtime.agent_registry.roster_valid = true;
    assert!(matches!(
        resolve(
            &mut h,
            Some("unknown-role"),
            Mode::Ensure {
                preferred_agent_id: None
            }
        ),
        Outcome::Error {
            kind: ErrorKind::InvalidRole,
            ..
        }
    ));
    assert!(matches!(
        h.resolve_bridge_receiver(
            &crate::test_connection_id("bridge"),
            &BridgeReceiverRequest {
                request_id: "wrong-session".to_owned(),
                session_id: test_session_id("other"),
                role: Some("engineer".to_owned()),
                mode: Mode::Ensure {
                    preferred_agent_id: None
                }
            },
            &h.current_extension_frame_admission(),
        ),
        Outcome::Error {
            kind: ErrorKind::InvalidRequest,
            ..
        }
    ));
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    let id = selected(resolve(
        &mut h,
        Some("engineer"),
        Mode::Ensure {
            preferred_agent_id: None,
        },
    ));
    assert!(
        h.session_runtime
            .agent_store
            .agent_persistence(id.as_str())
            .is_ephemeral()
    );
    assert!(Harness::is_receiving_endpoint(
        &h.agent_runtime.agent_registry.agents[&id]
    ));
    assert!(!td.path().join("agents").join(id.as_str()).exists());
    h.shutdown().expect("shutdown");
}

/// An intercepted creation already owns its future endpoint; repeated ensure
/// cannot create a second agent while creation/membership publication settles.
#[test]
fn bridge_receiver_pending_creation_is_singleflight_without_bootstrap() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    let _sink = connect_test_tool(&mut h, "creation-interceptor");
    h.handle_extension_event(
        "creation-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let mode = Mode::Ensure {
        preferred_agent_id: None,
    };
    assert_eq!(
        resolve(&mut h, Some("engineer"), mode.clone()),
        Outcome::Unavailable {
            reason: Unavailable::NotReady
        }
    );
    assert_eq!(
        resolve(&mut h, Some("engineer"), mode.clone()),
        Outcome::Unavailable {
            reason: Unavailable::NotReady
        }
    );
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
    h.handle_extension_event(
        "creation-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("protected creation");
    let id = selected(resolve(&mut h, Some("engineer"), mode));
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(id.as_str())
            .expect("events")
            .iter()
            .all(|record| !matches!(record.event, Event::AgentPromptSubmitted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Tool-backed children are ordinary eligible agents, but their immutable role
/// still constrains designation independently of the parent and runtime label.
#[test]
fn bridge_receiver_child_and_role_constraints() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "test-bridge");
    let parent = h.create_durable_user_agent(test_session_id("s1"), "engineer");
    let call = ToolCallId::new("child-start");
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call.clone(), parent.clone());
    let role = h.config.available_roles["engineer"].clone();
    h.config
        .available_roles
        .insert("other-role".to_owned(), role);
    let pending = h
        .prepare_start_agent_request(
            crate::harness::harness_connection_id(),
            tau_proto::StartAgentRequest {
                query_id: "child".to_owned(),
                instruction: String::new(),
                role: Some("engineer".to_owned()),
                tool_call_id: Some(call.clone()),
                trusted_internal_spans: vec![],
                input_stats: Default::default(),
                task_name: None,
                parent_agent: Some(parent.clone()),
            },
        )
        .expect("prepare")
        .expect("new child");
    let child = pending.cid.clone();
    h.start_agent_request_inner(pending, false, None, false, None)
        .expect("child");
    assert_eq!(
        selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Restore {
                agent_id: Some(child.clone())
            }
        )),
        child
    );
    assert_eq!(
        resolve(
            &mut h,
            Some("other-role"),
            Mode::Restore {
                agent_id: Some(child.clone())
            }
        ),
        Outcome::Unavailable {
            reason: Unavailable::NoEligibleAgent
        }
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call.clone(), crate::test_connection_id("bridge"));
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call.clone(), child);
    assert!(matches!(
        resolve(
            &mut h,
            Some("other-role"),
            Mode::Register {
                tool_call_id: call.clone()
            }
        ),
        Outcome::Error {
            kind: ErrorKind::InvalidCaller,
            ..
        }
    ));
    h.tool_routing.tool_runtime.tool_agents.remove(&call);
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .remove(&call);
    h.shutdown().expect("shutdown");
}

/// The directed bridge operation returns a private concrete target, and an
/// ordinary response on that target does not complete/unload a side query.
#[test]
fn bridge_receiver_directed_fixture_retains_endpoint_after_response() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let frames = connect_ready_configured_extension(
        &mut h,
        "bridge",
        "test-bridge",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("bridge")
        .expect("bridge")
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);
    h.handle_extension_message(
        &crate::test_connection_id("bridge"),
        HarnessInputMessage::BridgeReceiverRequest(BridgeReceiverRequest {
            request_id: "directed".to_owned(),
            session_id: test_session_id("s1"),
            role: Some("engineer".to_owned()),
            mode: Mode::Ensure {
                preferred_agent_id: None,
            },
        }),
    )
    .expect("receiver RPC through configured ingress");
    let id = frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match &frame.frame {
            HarnessOutputMessage::BridgeReceiverResult(result)
                if result.request_id == "directed" =>
            {
                Some(selected(result.outcome.clone()))
            }
            _ => None,
        })
        .expect("private result");
    h.handle_extension_event(
        "bridge",
        TestProtocolItem::Event(Event::ExtInternalPromptSubmitRequest(
            tau_proto::ExtInternalPromptSubmitRequest {
                agent_id: id.clone(),
                text: "ordinary receiving turn".to_owned(),
                ctx_id: None,
                activation_kind: None,
            },
        )),
    )
    .expect("input");
    let prompt_id = h.agent_runtime.agent_registry.agents[&id]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("ordinary inference");
    let mut response = super::dispatch::provider_text_response(&prompt_id, id.clone(), "done");
    response.originator = h.agent_runtime.agent_registry.agents[&id]
        .identity
        .originator
        .clone();
    h.handle_provider_response_finished(response)
        .expect("response");
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&id));
    assert_eq!(
        selected(resolve(
            &mut h,
            Some("engineer"),
            Mode::Restore {
                agent_id: Some(id.clone())
            }
        )),
        id
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::StartAgentResult(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Build the same explicit-role request for live and activation-deferred
/// ingress.
fn ingress_ensure() -> HarnessInputMessage {
    HarnessInputMessage::BridgeReceiverRequest(BridgeReceiverRequest {
        request_id: "ingress".to_owned(),
        session_id: test_session_id("s1"),
        role: Some("engineer".to_owned()),
        mode: Mode::Ensure {
            preferred_agent_id: None,
        },
    })
}

/// Hold post-Ready operational traffic behind an actual intercepted
/// declaration.
fn park_bridge_activation(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let frames = super::lifecycle::connect_handshaking_tool(h, "bridge");
    h.extensions
        .entries
        .get_mut("bridge")
        .expect("bridge")
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);
    connect_test_tool(h, "activation-interceptor");
    h.handle_extension_message(
        &crate::test_connection_id("activation-interceptor"),
        HarnessInputMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            )],
            priority: InterceptionPriority::new(0),
        }),
    )
    .expect("intercept declaration");
    h.handle_extension_message(
        &crate::test_connection_id("bridge"),
        HarnessInputMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtPromptFragmentPublish(
                tau_proto::ExtPromptFragmentPublish {
                    fragment: tau_proto::PromptFragment::new(
                        "bridge.activation",
                        tau_proto::PromptPriority::new(10),
                        "bridge instructions",
                    ),
                },
            )),
            persist: true,
        }),
    )
    .expect("park declaration");
    h.handle_extension_message(
        &crate::test_connection_id("bridge"),
        HarnessInputMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("Ready");
    assert!(h.extensions.ready_received.contains("bridge"));
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    h.handle_extension_message(&crate::test_connection_id("bridge"), ingress_ensure())
        .expect("defer receiver RPC");
    assert_eq!(
        h.extensions.activation_staging["bridge"]
            .deferred_messages
            .len(),
        1,
        "RPC must really wait behind activation"
    );
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    frames
}

/// Read only this RPC's private outcomes, excluding replay and lifecycle
/// traffic.
fn ingress_outcomes(frames: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<Outcome> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .filter_map(|frame| match &frame.frame {
            HarnessOutputMessage::BridgeReceiverResult(result)
                if result.request_id == "ingress" =>
            {
                Some(result.outcome.clone())
            }
            _ => None,
        })
        .collect()
}

/// Ready-but-not-activated requests must replay through legal ingress exactly
/// once.
#[test]
fn bridge_receiver_ingress_replays_after_activation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let frames = park_bridge_activation(&mut h);
    h.handle_extension_message(
        &crate::test_connection_id("activation-interceptor"),
        HarnessInputMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        }),
    )
    .expect("release activation");
    let outcomes = ingress_outcomes(&frames);
    assert_eq!(outcomes.len(), 1);
    let id = selected(outcomes[0].clone());
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&id));
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
    h.shutdown().expect("shutdown");
}

/// Shutdown may release activation while quiescing declarations, but that
/// release must not turn an old-generation queued Ensure into a newly created
/// receiver.
#[test]
fn bridge_receiver_ingress_shutdown_rejects_deferred_ensure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let frames = park_bridge_activation(&mut h);
    h.shutdown().expect("shutdown");
    assert!(
        !h.extensions.activation_staging.contains_key("bridge"),
        "shutdown must actually release the parked activation"
    );
    assert_eq!(
        ingress_outcomes(&frames),
        vec![Outcome::Unavailable {
            reason: Unavailable::NoEligibleAgent,
        }],
        "the deferred RPC must reach its stale-admission rejection"
    );
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStarted(_) | Event::SessionAgentLoaded(_)))
    );
}

/// An operational receiver request before Ready cannot bypass phase validation.
#[test]
fn bridge_receiver_ingress_before_ready_cannot_create() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let frames = super::lifecycle::connect_handshaking_tool(&mut h, "bridge");
    h.extensions
        .entries
        .get_mut("bridge")
        .expect("bridge")
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);
    let _ = h.handle_extension_message(&crate::test_connection_id("bridge"), ingress_ensure());
    assert!(ingress_outcomes(&frames).is_empty());
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    assert!(!h.extensions.activation_staging.contains_key("bridge"));
    h.shutdown().expect("shutdown");
}

/// Terminal shutdown rejects even a newly captured request without changing the
/// existing malformed-request and wrong-session error classification.
#[test]
fn bridge_receiver_ingress_after_shutdown_cannot_create() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let frames = connect_ready_configured_extension(
        &mut h,
        "bridge",
        "test-bridge",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("bridge")
        .expect("bridge")
        .peer_capabilities
        .insert(tau_proto::PeerCapability::MessageBridge);
    assert!(h.publish_current_session_shutdown());
    h.handle_extension_message(&crate::test_connection_id("bridge"), ingress_ensure())
        .expect("terminal request");
    assert_eq!(
        ingress_outcomes(&frames),
        vec![Outcome::Unavailable {
            reason: Unavailable::NoEligibleAgent,
        }]
    );
    for (request_id, session_id) in [
        ("", test_session_id("s1")),
        ("wrong-session", test_session_id("other")),
    ] {
        h.handle_extension_message(
            &crate::test_connection_id("bridge"),
            HarnessInputMessage::BridgeReceiverRequest(BridgeReceiverRequest {
                request_id: request_id.to_owned(),
                session_id,
                role: Some("engineer".to_owned()),
                mode: Mode::Ensure {
                    preferred_agent_id: None,
                },
            }),
        )
        .expect("invalid request");
        assert!(frames.lock().expect("frames").iter().any(|frame| {
            matches!(&frame.frame, HarnessOutputMessage::BridgeReceiverResult(result)
            if result.request_id == request_id
                && matches!(result.outcome, Outcome::Error {
                    kind: ErrorKind::InvalidRequest, ..
                }))
        }));
    }
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    h.shutdown().expect("shutdown");
}
