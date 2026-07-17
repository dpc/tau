use super::dispatch::{context_overflow_response, provider_text_response};
use super::*;
use crate::harness::{PendingTool, background_completion_prompt};

/// Construct one forged-provenance fact for direct extension intake.
fn extension_message_fact(message_id: &str) -> Event {
    Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("forged"),
        tau_proto::MessageAgentTarget::new("invalid target"),
        tau_proto::MessageFactId::new(message_id),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
        },
        None,
        "hello",
    ))
}

/// Assert one selector cannot observe a pre-commit interception request or
/// alter the exact stamped canonical fact.
fn assert_message_fact_bypasses_interceptor(selector: EventSelector) {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let interceptor_sink = connect_test_tool(&mut h, "message-interceptor");
    h.handle_extension_event(
        "message-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![selector],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    connect_ready_message_publisher(&mut h, "bridge-connection", "configured-bridge");

    h.handle_extension_event_inner_with_transient(
        "bridge-connection",
        extension_message_fact("m1"),
        Some(true),
    )
    .expect("message fact intake");

    assert!(h.pending_intercept.is_none());
    assert!(
        interceptor_sink
            .lock()
            .expect("interceptor sink")
            .iter()
            .all(|frame| !matches!(frame.frame, HarnessOutputMessage::InterceptRequest(_))),
        "message fact must never produce an InterceptRequest"
    );
    let records = h.store.session_events("s1").expect("fallback records");
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0].event,
        Event::MessageDelivered(fact)
            if fact.publisher_extension_id.as_str() == "configured-bridge"
                && fact.message_id.as_str() == "m1"
    ));
}

/// An exact message selector cannot intercept a direct message fact.
#[test]
fn message_fact_bypasses_exact_interceptor() {
    assert_message_fact_bypasses_interceptor(EventSelector::Exact(
        tau_proto::EventName::MESSAGE_DELIVERED,
    ));
}

/// A category prefix selector cannot intercept a direct message fact.
#[test]
fn message_fact_bypasses_prefix_interceptor() {
    assert_message_fact_bypasses_interceptor(EventSelector::Prefix("message".to_owned()));
}

/// A direct fact arriving behind an unrelated intercepted publish waits for
/// that earlier publish, then appends without entering its own interception
/// chain.
#[test]
fn message_fact_bypass_preserves_deferred_publish_fifo() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "draft-interceptor");
    h.handle_extension_event(
        "draft-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    connect_ready_message_publisher(&mut h, "bridge-connection", "configured-bridge");
    let observer = connect_test_client(&mut h, "fifo-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "fifo-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
            ],
        })),
    )
    .expect("subscribe observer");
    h.publish_event(None, draft_event("held"));
    assert!(h.pending_intercept.is_some());

    h.handle_extension_event_inner_with_transient(
        "bridge-connection",
        extension_message_fact("m1"),
        None,
    )
    .expect("queue message fact");
    assert!(
        h.store
            .session_events("s1")
            .expect("fallback records")
            .is_empty()
    );

    h.handle_extension_event(
        "draft-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release intercepted publish");

    assert_eq!(
        h.store
            .session_events("s1")
            .expect("fallback records")
            .len(),
        1
    );
    assert!(
        interceptor
            .lock()
            .expect("interceptor sink")
            .iter()
            .filter(|frame| matches!(frame.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count()
            == 1,
        "only the earlier draft may be intercepted"
    );
    let delivered_names = observer
        .lock()
        .expect("observer sink")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame).map(Event::name))
        .filter(|name| {
            *name == tau_proto::EventName::UI_PROMPT_DRAFT
                || *name == tau_proto::EventName::MESSAGE_DELIVERED
        })
        .collect::<Vec<_>>();
    assert_eq!(
        delivered_names,
        vec![
            tau_proto::EventName::UI_PROMPT_DRAFT,
            tau_proto::EventName::MESSAGE_DELIVERED
        ]
    );
}

fn prompt_created_count(h: &Harness) -> u64 {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut count = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if matches!(entry.event, Event::AgentPromptCreated(_)) {
            count += 1;
        }
    }
    count
}

fn add_second_test_model(h: &mut Harness) {
    let first: tau_proto::ModelId = "echo/model".into();
    let second: tau_proto::ModelId = "other/model".into();
    let mut info = h.provider_model_info[&first].clone();
    info.id = second.clone();
    let route = h.provider_model_routes[&first].clone();
    h.provider_model_info.insert(second.clone(), info);
    h.provider_model_routes.insert(second, route);
}

fn queue_intercepted_peer_receive(
    h: &mut Harness,
    connection_id: &tau_proto::ConnectionId,
    recipient_id: tau_proto::AgentId,
    suffix: &str,
) {
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id.clone(),
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: format!("peer-request-{suffix}"),
            message_id: format!("peer-message-{suffix}").into(),
            capability: "test-capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none(), "success must wait for receive commit");
}

fn committed_peer_receives(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(received)
                if received.sender_session_id.as_deref() == Some("sender-session") =>
            {
                Some(received)
            }
            _ => None,
        })
        .collect()
}

/// Remote success remains pending while interception parks the exact receive
/// projection and is released only by the post-persistence commit reaction.
#[test]
fn peer_receive_ack_waits_for_intercepted_projection_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-receive-interceptor");
    h.handle_extension_event(
        "peer-receive-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");

    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "commit");

    assert_eq!(h.pending_external_receive_acks.len(), 1);
    assert!(committed_peer_receives(&h).is_empty());
    h.handle_extension_event(
        "peer-receive-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");
    assert!(h.pending_external_receive_acks.is_empty());
    assert_eq!(committed_peer_receives(&h).len(), 1);
}

/// An interceptor rejection fails and removes the live continuation rather than
/// acknowledging or committing a receive projection.
#[test]
fn peer_receive_interception_drop_never_acknowledges_or_commits() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-drop-interceptor");
    h.handle_extension_event(
        "peer-drop-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");

    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "drop");
    h.handle_extension_event(
        "peer-drop-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Recipient disappearance while a receive is parked invalidates the
/// continuation at commit-time and cannot produce a durable receive.
#[test]
fn peer_receive_target_disappearance_before_commit_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-target-interceptor");
    h.handle_extension_event(
        "peer-target-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "target-gone");

    h.remove_agent(&cid);
    h.handle_extension_event(
        "peer-target-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Current-session bare routing delays its sent projection until the exact
/// receive projection passes interception and commits.
#[test]
fn local_peer_sent_projection_waits_for_receive_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint::default()),
    });
    let cid = ensure_test_user_agent(&mut h);
    let _interceptor = connect_test_tool(&mut h, "local-peer-interceptor");
    h.handle_extension_event(
        "local-peer-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.publish_peer_entrypoint_message_from_agent(
        &cid,
        "local peer body".to_owned(),
        "local-peer-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local peer");

    assert!(h.pending_external_receive_acks.len() == 1);
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
    h.handle_extension_event(
        "local-peer-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");
    assert!(h.pending_external_receive_acks.is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
}

/// Current-session bare routing enforces the same 64 KiB body limit as socket
/// routing before admission or auto-start creation.
#[test]
fn local_peer_oversized_message_rejects_before_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "peer".to_owned(),
        roles: vec!["peer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint {
            auto_start_role: Some("peer".to_owned()),
        }),
    });
    let agents_before = h.agents.len();

    let error = h
        .publish_peer_entrypoint_message_from_agent(
            &sender,
            "x".repeat(64 * 1024 + 1),
            "oversized-local-peer".into(),
            ToolName::new("message"),
            tau_proto::ToolType::Function,
        )
        .expect_err("oversized local peer message rejected");

    assert_eq!(error, "peer message exceeds the 64 KiB limit");
    assert_eq!(h.agents.len(), agents_before);
    assert!(h.pending_external_receive_acks.is_empty());
}

/// Current-session routing uses the same admission, auto-start, and post-commit
/// completion path as remote routing, while preserving local sender provenance.
#[test]
fn local_peer_auto_start_reports_started_only_after_receive_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "peer".to_owned(),
        roles: vec!["peer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint {
            auto_start_role: Some("peer".to_owned()),
        }),
    });
    let _interceptor = connect_test_tool(&mut h, "local-auto-start-interceptor");
    h.handle_extension_event(
        "local-auto-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.publish_peer_entrypoint_message_from_agent(
        &sender,
        "auto-start body".to_owned(),
        "local-auto-start-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local auto-start");

    let pending = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("pending receive");
    assert!(pending.started);
    let recipient_id = pending.recipient_id.clone();
    let recipient_cid = h
        .agent_routes
        .get(recipient_id.as_str())
        .expect("auto-started route");
    let recipient = &h.agents[recipient_cid];
    assert_eq!(recipient.role.as_deref(), Some("peer"));
    assert_eq!(recipient.parent_agent_id, None);
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );

    h.handle_extension_event(
        "local-auto-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
}

/// A parked local auto-start is visible to a remote send immediately, so both
/// precommit deliveries coalesce on one endpoint rather than creating fan-out.
#[test]
fn parked_local_and_remote_peer_sends_coalesce_on_one_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "peer".to_owned(),
        roles: vec!["peer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint {
            auto_start_role: Some("peer".to_owned()),
        }),
    });
    let _interceptor = connect_test_tool(&mut h, "coalesce-interceptor");
    h.handle_extension_event(
        "coalesce-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_peer_entrypoint_message_from_agent(
        &sender,
        "local first".to_owned(),
        "coalesce-local-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local auto-start");
    let recipient = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("local pending")
        .recipient_id
        .clone();
    let connection_id = tau_proto::ConnectionId::from("coalesce-peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let remote = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "coalesce-remote".to_owned(),
            message_id: "coalesce-remote-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "remote second".to_owned(),
        },
        Ok(()),
    );

    assert!(remote.is_none());
    assert_eq!(h.agents.len(), 2, "sender plus exactly one peer endpoint");
    assert_eq!(h.pending_external_receive_acks.len(), 2);
    assert!(
        h.pending_external_receive_acks
            .values()
            .all(|pending| pending.recipient_id == recipient)
    );
    assert_eq!(
        h.pending_external_receive_acks
            .values()
            .filter(|pending| pending.started)
            .count(),
        1
    );
}

/// Failed callback correlation cannot consume auto-start authority or create an
/// endpoint, even when the target explicitly configured an auto-start role.
#[test]
fn peer_auto_start_authentication_failure_precedes_spend() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint {
            auto_start_role: Some("engineer".to_owned()),
        }),
    });
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "auth-before-spend".to_owned(),
        message_id: "auth-before-spend-message".into(),
        capability: "invalid".to_owned(),
        sender_session_id: "sender-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "must not create".to_owned(),
    };

    let result = h
        .complete_external_agent_message_auth(
            "peer-client".into(),
            h.current_session_generation,
            request,
            Err("external message authentication failed".to_owned()),
        )
        .expect("terminal authentication result");

    assert_eq!(
        result.error.as_deref(),
        Some("external message authentication failed")
    );
    assert!(h.agents.is_empty());
    assert!(h.pending_external_receive_acks.is_empty());
}

/// A callback completion that outlives its session generation or peer socket is
/// rejected before selection, admission, or auto-start creation.
#[test]
fn stale_or_disconnected_auth_completion_cannot_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint {
            auto_start_role: Some("engineer".to_owned()),
        }),
    });
    let target_session = h.current_session_id.clone();
    let request = |suffix: &str| tau_proto::ExternalAgentMessageRequest {
        request_id: format!("stale-auth-{suffix}"),
        message_id: format!("stale-auth-message-{suffix}").into(),
        capability: "valid".to_owned(),
        sender_session_id: "sender-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: target_session.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "must not create".to_owned(),
    };
    let peer: tau_proto::ConnectionId = "peer-client".into();
    h.external_message_peers.insert(peer.clone());
    let stale = h
        .complete_external_agent_message_auth(
            peer.clone(),
            h.current_session_generation.saturating_add(1),
            request("generation"),
            Ok(()),
        )
        .expect("stale generation result");
    h.external_message_peers.remove(&peer);
    let disconnected = h
        .complete_external_agent_message_auth(
            peer,
            h.current_session_generation,
            request("disconnect"),
            Ok(()),
        )
        .expect("disconnected result");

    assert!(stale.error.is_some());
    assert!(disconnected.error.is_some());
    assert!(h.agents.is_empty());
    assert!(h.pending_external_receive_acks.is_empty());
}

/// A canceled local continuation released after rollover retires silently and
/// cannot publish old receive/sent/tool terminal facts into the new session.
#[test]
fn local_peer_parked_across_rollover_has_no_stale_terminal() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint::default()),
    });
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "local-rollover-call".into();
    h.tool_agents.insert(call_id.clone(), cid.clone());
    let _interceptor = connect_test_tool(&mut h, "local-rollover-interceptor");
    h.handle_extension_event(
        "local-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_peer_entrypoint_message_from_agent(
        &cid,
        "old-session peer body".to_owned(),
        call_id.clone(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local peer");

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    h.handle_extension_event(
        "local-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release old receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::AgentMessageSent(message) if message.message == "old-session peer body")
            || matches!(event, Event::AgentMessageReceived(message) if message.message == "old-session peer body")
            || matches!(event, Event::ToolResult(result) if result.call_id == call_id)
            || matches!(event, Event::ToolError(error) if error.call_id == call_id)
    }));
}

/// Bare entrypoint authority is revalidated at the persistence boundary, so a
/// parked receive cannot commit after the target policy is revoked.
#[test]
fn peer_receive_bare_authority_revocation_before_commit_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint::default()),
    });
    ensure_test_user_agent(&mut h);
    let _interceptor = connect_test_tool(&mut h, "bare-revoke-interceptor");
    h.handle_extension_event(
        "bare-revoke-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "bare-revoke".to_owned(),
            message_id: "bare-revoke-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none());

    h.peer_entrypoint = None;
    h.handle_extension_event(
        "bare-revoke-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Bare routing gets only one deterministic re-selection: invalidating the
/// replacement fails terminally without a third selection or committed receive.
#[test]
fn peer_receive_bare_target_loss_reselects_once_before_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.peer_entrypoint = Some(tau_config::settings::RoleGroup {
        name: "engineer".to_owned(),
        roles: vec!["engineer".to_owned()],
        peer_entrypoint: Some(tau_config::settings::PeerEntrypoint::default()),
    });
    ensure_test_user_agent(&mut h);
    h.create_durable_user_agent("s1".into(), "engineer");
    let _interceptor = connect_test_tool(&mut h, "bare-reselect-interceptor");
    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "bare-reselect".to_owned(),
            message_id: "bare-reselect-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none());
    let original = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("pending receive")
        .recipient_id
        .clone();
    let original_cid = h
        .agent_routes
        .get(original.as_str())
        .cloned()
        .expect("original route");
    h.remove_agent(&original_cid);

    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release stale receive");
    assert_eq!(h.pending_external_receive_acks.len(), 1);
    assert!(committed_peer_receives(&h).is_empty());
    let replacement = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("replacement receive")
        .recipient_id
        .clone();
    assert_ne!(replacement, original);
    let replacement_cid = h
        .agent_routes
        .get(replacement.as_str())
        .cloned()
        .expect("replacement route");
    h.remove_agent(&replacement_cid);

    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release invalid replacement receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
    assert!(
        h.agent_routes.is_empty(),
        "second invalidation must not reselect"
    );
}

/// A parked old-generation receive retains a canceled tombstone across rollover
/// and is rejected when the interceptor later releases it.
#[test]
fn peer_receive_parked_across_rollover_cannot_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-rollover-interceptor");
    h.handle_extension_event(
        "peer-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "rollover");

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    assert!(
        h.pending_external_receive_acks
            .values()
            .all(|pending| pending.canceled)
    );
    h.handle_extension_event(
        "peer-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release old receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// A final response parked before commit must not fan out watch content after
/// removing the watched sender has made that endpoint non-live.
#[test]
fn intercepted_final_response_cannot_fan_out_after_watched_agent_unload() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let interceptor = connect_test_tool(&mut h, "watch-final-interceptor");
    h.handle_extension_event(
        "watch-final-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_for_agent(
        &watched_cid,
        Event::ProviderResponseFinished(provider_text_response(
            &"sp-parked-watch-final".into(),
            crate::parse_agent_id(&watched_id),
            "must not cross unload",
        )),
    );
    let (parked, _) = intercepted_payload(&interceptor);
    assert!(matches!(parked, Event::ProviderResponseFinished(_)));

    h.remove_agent(&watched_cid);
    h.handle_extension_event(
        "watch-final-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release final response");

    assert!(
        event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::AgentMessageReceived(message)
                if message.kind == tau_proto::AgentMessageKind::WatchResponse
                    && message.recipient_id.as_str() == watcher_id
        )),
        "parked final response must not append watch content after unload"
    );
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    h.shutdown().expect("shutdown");
}

/// A checkpoint parked before commit owns its provider-qualified model even if
/// `/model` timing changes the loaded agent before prompt materialization.
#[test]
fn intercepted_inference_checkpoint_pins_materialized_model() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    add_second_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let interceptor = connect_test_tool(&mut h, "checkpoint-model-owner");
    h.handle_extension_event(
        "checkpoint-model-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("owned by A".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentInferenceDispatchStarted(checkpoint) = parked else {
        panic!("checkpoint intercepted");
    };
    assert_eq!(checkpoint.model, Some("echo/model".into()));
    h.agents.get_mut(&cid).expect("agent").model_override = Some("other/model".into());
    h.handle_extension_event(
        "checkpoint-model-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release checkpoint");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.agent_prompt_id, checkpoint.agent_prompt_id);
    assert_eq!(prompt.model, checkpoint.model.expect("qualified model"));
    h.shutdown().expect("shutdown");
}

/// If a checkpoint's captured route disappears while interception parks the
/// materialized prompt, commit excludes providers and durably terminalizes it.
#[test]
fn intercepted_inference_checkpoint_fails_before_unroutable_send() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let provider_observer =
        connect_test_client(&mut h, "unowned-provider", tau_proto::ClientKind::Provider);
    h.bus
        .set_subscriptions(
            "unowned-provider",
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
        )
        .expect("subscribe provider observer");
    let interceptor = connect_test_tool(&mut h, "checkpoint-route-owner");
    h.handle_extension_event(
        "checkpoint-route-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("route vanishes".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentPromptCreated(prompt) = parked else {
        panic!("materialized prompt intercepted");
    };
    h.provider_model_routes.remove(&prompt.model);
    h.handle_extension_event(
        "checkpoint-route-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release checkpoint");

    assert!(
        !provider_observer
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(created))
                    if created.agent_prompt_id == prompt.agent_prompt_id
            )),
        "an unroutable owned prompt must not be broadcast to providers"
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == prompt.agent_prompt_id
                && response.stop_reason == tau_proto::ProviderStopReason::Error
    )));
    h.shutdown().expect("shutdown");
}

/// A standalone start parked before commit owns the compact request model even
/// if model selection changes before its post-commit provider dispatch.
#[test]
fn intercepted_compaction_start_pins_materialized_model() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    add_second_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    {
        let info = h
            .provider_model_info
            .get_mut(&tau_proto::ModelId::from("echo/model"))
            .expect("model");
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(1);
        let agent = h.agents.get_mut(&cid).expect("agent");
        agent.context_input_tokens = Some(1);
        agent.context_usage_head = agent.head;
        agent.context_usage_model = Some("echo/model".into());
    }
    let interceptor = connect_test_tool(&mut h, "compact-model-owner");
    h.handle_extension_event(
        "compact-model-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    assert!(h.schedule_standalone_auto_compaction(&cid));
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentStandaloneCompactionStarted(started) = parked else {
        panic!("compaction start intercepted");
    };
    assert_eq!(started.model, tau_proto::ModelId::from("echo/model"));
    h.agents.get_mut(&cid).expect("agent").model_override = Some("other/model".into());
    h.handle_extension_event(
        "compact-model-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release start");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.agent_prompt_id, started.compact_prompt_id);
    assert_eq!(prompt.model, started.model);
    assert_eq!(
        prompt.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    h.shutdown().expect("shutdown");
}

/// A replay-drift claim parked by interception must remain uniquely pending;
/// after commit its suppression is consumed and the correlated failure blocks
/// recovery without ever creating a compact provider prompt.
#[test]
fn intercepted_reactive_drift_terminalization_never_dispatches() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint)
                if checkpoint.agent_prompt_id == inference.agent_prompt_id =>
            {
                Some(checkpoint)
            }
            _ => None,
        })
        .expect("checkpoint");
    let mut planned = context_overflow_response(&inference);
    planned.recovery_disposition = tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
    h.publish_for_agent(&cid, Event::ProviderResponseFinished(planned));
    h.agents.get_mut(&cid).expect("agent").activation_dispatch =
        crate::agent::ActivationDispatchState::ContextRecoveryPending {
            checkpoint: checkpoint.clone(),
        };

    let interceptor = connect_test_tool(&mut h, "reactive-start-interceptor");
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.reconcile_pending_context_recoveries(false);
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::ContextRecoveryClaimPending { .. }
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        0
    );
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release start");
    assert!(h.suppressed_compaction_dispatches.is_empty());
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        0
    );
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::Blocked { .. }
    ));
    h.shutdown().expect("shutdown");
}

/// Real interception replies cannot flip the harness-owned activation bit in
/// either direction on any canonical transcript fact.
#[test]
fn interception_rejects_activation_bit_forgery_for_all_canonical_facts() {
    for inference_activation in [false, true] {
        let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
        let cases = [
            (
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation,
                    agent_id: agent_id.clone(),
                    text: "submitted".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    originator: tau_proto::PromptOriginator::User,
                    submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                    display_name: None,
                    ctx_id: None,
                }),
            ),
            (
                tau_proto::EventName::AGENT_USER_MESSAGE_INJECTED,
                Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                    inference_activation,
                    agent_id: agent_id.clone(),
                    text: "injected".to_owned(),
                    message_class: tau_proto::PromptMessageClass::Internal,
                }),
            ),
            (
                tau_proto::EventName::AGENT_PROMPT_STEERED,
                Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                    inference_activation,
                    agent_id,
                    text: "steered".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    ctx_id: None,
                }),
            ),
        ];

        for (event_name, original) in cases {
            let tmp = TempDir::new().expect("tempdir");
            let mut h = echo_harness(tmp.path()).expect("harness");
            let cid = ensure_test_user_agent(&mut h);
            let _interceptor = connect_test_tool(&mut h, "activation-rewriter");
            h.handle_extension_event(
                "activation-rewriter",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(event_name)],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept registration");

            h.publish_for_agent(&cid, original.clone());
            let mut replacement = original.clone();
            match &mut replacement {
                Event::AgentPromptSubmitted(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                Event::AgentUserMessageInjected(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                Event::AgentPromptSteered(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                _ => unreachable!(),
            }
            h.handle_extension_event(
                "activation-rewriter",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(Some(Box::new(replacement.clone()))),
                })),
            )
            .expect("intercept reply");

            let events = event_log_events(&h);
            assert!(events.contains(&original));
            assert!(!events.contains(&replacement));
        }
    }
}

/// Sink that rejects intercepted frames to exercise failed-delivery recovery.
struct FailingInterceptSink;

impl ConnectionSink for FailingInterceptSink {
    fn send(&mut self, _event: RoutedFrame) -> Result<(), ConnectionSendError> {
        Err(ConnectionSendError::new("test sink closed"))
    }
}

fn connect_named_test_tool(
    h: &mut Harness,
    connection_id: &str,
    component_name: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: connection_id.into(),
            name: component_name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

fn connect_failing_test_tool(h: &mut Harness, name: &str) {
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(FailingInterceptSink),
    ));
}

#[test]
fn interception_exact_selector_intercepts_before_log() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    let start_seq = h.event_log.next_seq();

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("held"));

    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, draft_event("held"));
    assert!(
        transient,
        "UiPromptDraft default transient flag is preserved"
    );
    assert_eq!(h.event_log.next_seq(), after_registration_seq);
    assert!(after_registration_seq.get() < start_seq.get() + 2);
}

#[test]
fn interception_drop_prevents_final_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    // UiPromptDraft is not on the must-pass list, so an explicit Drop
    // really does drop it.
    h.publish_event(None, draft_event("dropped"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    assert_eq!(h.event_log.next_seq(), after_registration_seq);
}

#[test]
fn interception_pass_through_reaches_log_after_last_interceptor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("released"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("released event in log");
    assert_eq!(entry.event, draft_event("released"));
}

#[test]
fn interception_reply_can_modify_event() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("original"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(draft_event("modified")))),
        })),
    )
    .expect("modifying reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("modified event in log");
    assert_eq!(entry.event, draft_event("modified"));
}

#[test]
fn interception_cannot_modify_mandatory_harness_notice() {
    // Mandatory harness diagnostics include extension config parse failures.
    // Interceptors may observe them, but must not be able to blank or downgrade
    // the message and recreate the same silent-fallback failure for live UIs.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info_important("extension core-shell rejected its config");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: String::new(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("important info in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Warning
                && info.message == "extension core-shell rejected its config"
    ));
}

#[test]
fn interception_cannot_modify_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: "downgraded".to_owned(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_drop_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_escalate_non_mandatory_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info("ordinary notice");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_CONFIG_ERROR.to_owned(),
                    message: "edited message".to_owned(),
                    level: tau_proto::NoticeLevel::Critical,
                    always_show: true,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Info
                && info.kind == tau_proto::notice_kind::HARNESS_NOTICE
                && !info.always_show
                && info.message == "edited message"
    ));
}

#[test]
fn interception_priority_orders_lower_values_first() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let low = connect_test_tool(&mut h, "low");
    let high = connect_test_tool(&mut h, "high");
    for (name, priority) in [("low", 10), ("high", 0)] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(priority),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));

    assert!(
        high.lock()
            .expect("high events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !low.lock()
            .expect("low events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_same_priority_orders_by_component_name_and_redelivery_continues() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let alpha = connect_test_tool(&mut h, "alpha");
    let beta = connect_test_tool(&mut h, "beta");
    for name in ["beta", "alpha"] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("chain"));
    assert!(
        alpha
            .lock()
            .expect("alpha events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !beta
            .lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );

    h.handle_extension_event(
        "alpha",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("alpha pass");
    assert!(
        beta.lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_exact_beats_prefix_even_with_lower_prefix_priority() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let exact = connect_test_tool(&mut h, "exact");
    let prefix = connect_test_tool(&mut h, "prefix");
    h.handle_extension_event(
        "prefix",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("ui".to_owned())],
            priority: InterceptionPriority::new(-100),
        })),
    )
    .expect("prefix registration");
    h.handle_extension_event(
        "exact",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(100),
        })),
    )
    .expect("exact registration");

    h.publish_event(None, draft_event("exact"));

    assert!(
        exact
            .lock()
            .expect("exact events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !prefix
            .lock()
            .expect("prefix events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_pass_advances_past_responding_interceptor() {
    // With the new InterceptReply protocol the cursor lives on the
    // harness side and always advances strictly past the interceptor
    // that just replied. The old "Emit with interception: None
    // restarts" pattern is gone — a Pass(None) reply does *not* loop
    // the event back through the same interceptor.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    h.publish_event(None, draft_event("once"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let count = interceptor
        .lock()
        .expect("events")
        .iter()
        .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
        .count();
    assert_eq!(
        count, 1,
        "pass-through must not re-trigger the same interceptor"
    );
}

/// Ensures same-priority cursor advancement uses full registration order rather
/// than connection-id order alone.
#[test]
fn interception_cursor_uses_full_registration_order() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let first = connect_named_test_tool(&mut h, "z-conn", "alpha-component");
    let second = connect_named_test_tool(&mut h, "a-conn", "beta-component");

    for connection_id in ["z-conn", "a-conn"] {
        h.handle_extension_event(
            connection_id,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));
    h.handle_extension_event(
        "z-conn",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("first pass reply");

    assert_eq!(
        first
            .lock()
            .expect("first events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1
    );
    assert_eq!(
        second
            .lock()
            .expect("second events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1,
        "same-priority cursor must follow component-name ordering, not connection-id ordering"
    );
}

#[test]
fn interception_defers_subsequent_publishes_until_reply() {
    // Regression for the "Ready" loop: while one publish is parked
    // waiting on an InterceptReply, the harness must defer any
    // subsequent publishes rather than commit them out of order.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    // Publish two: the first parks in interception (matches the
    // selector); the second does NOT match and so would, in the
    // buggy world, race ahead of it.
    h.publish_event(None, draft_event("held"));
    h.publish_event(
        None,
        Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "test.info".to_owned(),
            message: "second".to_owned(),
            level: tau_proto::NoticeLevel::Info,
            always_show: false,
        }),
    );
    // Neither has committed yet — interception is in flight on the
    // first, the second is sitting in `deferred_publishes`.
    assert_eq!(h.event_log.next_seq(), baseline_seq);

    // Reply: pass-through. Both events should now commit, in order.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let first = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first event committed");
    assert_eq!(first.event, draft_event("held"));
    let second = h
        .event_log
        .get_next_from(first.seq.next())
        .expect("second event committed");
    assert!(matches!(
        &second.event,
        Event::HarnessNotice(info) if info.message == "second"
    ));
}

#[test]
fn deferred_tool_result_persists_after_call_tracking_is_cleared() {
    // Regression for a real rostra session failure. A tool result can
    // arrive while an unrelated event is parked in interception. The
    // result publish is deferred, but the intake path still completes
    // the call immediately and clears `tool_agents`. The
    // eventual deferred commit must persist to the conversation's
    // session from the publish snapshot, not from now-missing call
    // tracking; otherwise the next LLM prompt contains a tool_use
    // without its matching tool_result and the provider rejects it.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "call-read".into();
    let tool_name = ToolName::new("read");

    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-main".into(),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.publish_event(None, draft_event("held"));
    assert!(
        h.pending_intercept.is_some(),
        "draft publish should be parked in interception"
    );

    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("defer tool result");
    assert!(
        !h.tool_agents.contains_key(&call_id),
        "tool call tracking is cleared before the deferred publish commits"
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    let has_result = default_agent_branch(&h).iter().any(|entry| {
        matches!(
            entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item|
                    item.call_id == call_id && item.status == ToolResultStatus::Success
                )
        )
    });
    assert!(
        has_result,
        "deferred tool.result must persist despite cleared call tracking"
    );
}

#[test]
fn interception_drop_of_must_pass_event_is_overridden() {
    // AgentPromptSubmitted is on the MUST_PASS list — even if an
    // interceptor returns Drop, the harness must publish the
    // original event (with a warn).
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let prompt = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: true,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    h.publish_event(None, prompt.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("must-pass event still committed despite Drop");
    assert_eq!(entry.event, prompt);
}

fn agent_started_event(role: &str) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        parent_agent: None,
        agent_id: tau_proto::AgentId::parse("agent-started-test").expect("agent id"),
        role: role.to_owned(),
        display_name: Some("Started Test".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn persisted_agent_started_events(h: &Harness) -> Vec<Event> {
    h.agent_store
        .agent_events("agent-started-test")
        .expect("agent.started durable log")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Ensures interceptors cannot drop agent creation facts now that
/// AgentStarted flows through the central publish/interception pipeline.
#[test]
fn interception_drop_of_agent_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started still committed despite Drop");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

/// Ensures interceptors cannot rewrite immutable agent creation facts such as
/// the role attached to an AgentStarted event.
#[test]
fn interception_replacement_of_agent_started_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_started_event("reviewer")))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started committed");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

fn session_agent_loaded_event(agent_id: &str) -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}

fn session_agent_unloaded_event(agent_id: &str) -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
    })
}

/// Ensures interceptors cannot drop durable session membership load facts,
/// because resume state depends on the committed membership log matching live
/// delivery.
#[test]
fn interception_drop_of_session_agent_loaded_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let loaded = session_agent_loaded_event("agent-loaded-original");
    h.publish_event(None, loaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_loaded still committed despite Drop");
    assert_eq!(entry.event, loaded);
    let membership = h
        .store
        .session("session-intercept")
        .expect("session membership");
    assert!(
        membership
            .contains_agent(&tau_proto::AgentId::parse("agent-loaded-original").expect("agent id"))
    );
}

/// Ensures interceptors cannot rewrite durable session membership unload facts,
/// preventing one agent's unload from being persisted as another agent's
/// unload.
#[test]
fn interception_replacement_of_session_agent_unloaded_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_UNLOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let unloaded = session_agent_unloaded_event("agent-unloaded-original");
    h.publish_event(None, unloaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_agent_unloaded_event(
                "agent-unloaded-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_unloaded committed");
    assert_eq!(entry.event, unloaded);
    let events = h
        .store
        .session_events("session-intercept")
        .expect("session events")
        .into_iter()
        .map(|entry| entry.event)
        .collect::<Vec<_>>();
    assert_eq!(events, vec![unloaded]);
}

fn session_started_event(session_id: &str) -> Event {
    Event::SessionStarted(tau_proto::SessionStarted {
        session_id: session_id.into(),
        reason: tau_proto::SessionStartReason::New,
    })
}

fn session_shutdown_event(session_id: &str) -> Event {
    Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: session_id.into(),
    })
}

fn agent_message_sent_event(message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: tau_proto::AgentId::parse("agent-message-recipient").expect("agent id"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn agent_message_received_event(recipient_id: &str) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        sender_session_id: None,
        recipient_id: tau_proto::AgentId::parse(recipient_id).expect("agent id"),
        kind: tau_proto::AgentMessageKind::Message,
        watch_turn_state: None,
        watch_provider_status: None,
        message: "hello".to_owned(),
    })
}

/// Ensures interceptors cannot drop session lifecycle facts required by
/// extensions and context providers for per-session setup.
#[test]
fn interception_drop_of_session_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = session_started_event("session-lifecycle-original");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.started still committed despite Drop");
    assert_eq!(entry.event, started);
}

/// Ensures interceptors cannot rewrite session shutdown facts used to flush or
/// drop extension-owned per-session state.
#[test]
fn interception_replacement_of_session_shutdown_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let shutdown = session_shutdown_event("session-lifecycle-original");
    h.publish_event(None, shutdown.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_shutdown_event(
                "session-lifecycle-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.shutdown committed");
    assert_eq!(entry.event, shutdown);
}

/// Ensures interceptors cannot drop harness-validated sender-side message
/// projections after recipient validation has already succeeded.
#[test]
fn interception_drop_of_agent_message_sent_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let sent = agent_message_sent_event("hello");
    h.publish_event(None, sent.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_sent still committed despite Drop");
    assert_eq!(entry.event, sent);
}

/// Ensures interceptors cannot rewrite harness-validated recipient-side message
/// projections, including attempts to route the projection to another agent.
#[test]
fn interception_replacement_of_agent_message_received_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let received = agent_message_received_event("agent-message-recipient");
    h.publish_event(None, received.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_message_received_event(
                "agent-message-other-recipient",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_received committed");
    assert_eq!(entry.event, received);
}

fn tool_result_event(call_id: &str, text: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

fn tool_cancelled_event(call_id: &str) -> Event {
    Event::ToolCancelled(tau_proto::ToolCancelled {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
    })
}

/// Ensures interceptors cannot rewrite terminal tool transcript facts, because
/// changing the call id would detach the completion from the requested tool
/// use.
#[test]
fn interception_replacement_of_tool_result_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_RESULT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let result = tool_result_event("call-original", "ok");
    h.publish_event(None, result.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(tool_result_event(
                "call-rewritten",
                "rewritten",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.result committed");
    assert_eq!(entry.event, result);
}

/// Ensures interceptors cannot drop cancellation facts for terminal tool calls.
#[test]
fn interception_drop_of_tool_cancelled_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let cancelled = tool_cancelled_event("call-cancelled");
    h.publish_event(None, cancelled.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.cancelled still committed despite Drop");
    assert_eq!(entry.event, cancelled);
}

/// Ensures a failed intercept-request delivery does not park the publish
/// pipeline forever and subsequent publishes still commit.
#[test]
fn failed_intercept_request_delivery_skips_interceptor_and_drains_publishes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_failing_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let first = draft_event("first");
    let second = draft_event("second");
    h.publish_event(None, first.clone());
    h.publish_event(None, second.clone());

    assert!(h.pending_intercept.is_none());
    let first_entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first draft committed");
    assert_eq!(first_entry.event, first);
    let second_entry = h
        .event_log
        .get_next_from(first_entry.seq.next())
        .expect("second draft committed");
    assert_eq!(second_entry.event, second);
}

#[test]
fn interception_disconnect_mid_reply_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("inflight"));
    // Disconnect before the interceptor replies. The harness should
    // treat this as Pass(None) and still commit the event.
    h.handle_disconnect("interceptor");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("event committed after disconnect");
    assert_eq!(entry.event, draft_event("inflight"));
}

#[test]
fn interception_user_prompt_dispatch_waits_for_commit() {
    // Regression for the "Ready" loop. When `AgentPromptSubmitted` is
    // held in interception, the harness must not dispatch the agent
    // prompt against the pre-prompt conversation tail — the
    // assembled message list must include the just-committed user
    // message. We assert this by inspecting the conversation
    // head/tree before vs. after the intercept reply lands.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    let head_before_dispatch = h.agents.get(&cid).and_then(|c| c.head);
    let prompts_before = prompt_created_count(&h);

    // Drive the user-prompt path. The publish parks in interception.
    h.dispatch_prompt_for_agent(&cid, "real question".to_owned())
        .expect("dispatch");

    // While the intercept is in flight: no agent prompt was minted,
    // c.head hasn't moved, and the deferred-dispatch queue contains
    // our cid.
    assert_eq!(
        prompt_created_count(&h),
        prompts_before,
        "agent dispatch must wait until the prompt commits"
    );
    assert_eq!(
        h.agents.get(&cid).and_then(|c| c.head),
        head_before_dispatch,
        "c.head must not advance while the prompt is parked"
    );
    assert_eq!(h.pending_user_prompt_dispatches.len(), 1);

    // Reply pass-through. Commit + react fires the deferred
    // dispatch, and the AgentPromptCreated is built from the
    // updated tree.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    assert_eq!(h.pending_user_prompt_dispatches.len(), 0);
    assert_eq!(
        prompt_created_count(&h),
        prompts_before + 1,
        "agent dispatch fires once the prompt commits"
    );
    let head_after = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head_after);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items, .. }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "real question")
                )
        ),
        "c.head points at the just-committed user prompt"
    );
}

#[test]
fn passive_background_notice_and_user_prompt_dispatch_as_one_intercepted_batch() {
    // Regression: passive background notices published before a real user prompt
    // must not let interception wake provider dispatch before the user prompt
    // itself commits. The passive notice and user prompt are treated as one
    // publish batch and dispatch only after both intercepted submissions pass.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id);
    h.selected_model = Some("echo/model".into());
    let info = h
        .provider_model_info
        .get_mut(&"echo/model".into())
        .expect("echo model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(900);

    let _interceptor = connect_test_tool(&mut h, "interceptor-passive-batch");
    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h.agents.get_mut(&cid).expect("conversation");
        conv.context_input_tokens = Some(900);
        conv.context_usage_head = conv.head;
        conv.context_usage_model = Some("echo/model".into());
        conv.context_cached_tokens = Some(450);
    }
    let passive_text = background_completion_prompt(&"passive-intercept-bg".into());
    h.agents
        .get_mut(&cid)
        .expect("conversation")
        .pending_prompts
        .push_back(PendingPrompt::passive_background_completion(
            passive_text.clone(),
        ));
    let prompts_before = prompt_created_count(&h);

    h.dispatch_prompt_for_agent(&cid, "real follow-up".to_owned())
        .expect("dispatch user prompt with passive notice");

    assert_eq!(prompt_created_count(&h), prompts_before);
    assert_eq!(
        h.pending_publish_idle_dispatches.len(),
        1,
        "dispatch should be deferred for the whole passive+user batch"
    );

    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass passive notice");

    assert_eq!(
        prompt_created_count(&h),
        prompts_before,
        "provider dispatch must still wait for the real user prompt"
    );
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);

    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass user prompt");

    assert_eq!(h.pending_publish_idle_dispatches.len(), 0);
    assert_eq!(prompt_created_count(&h), prompts_before + 1);
    let compact = read_nth_prompt_created(&h, prompts_before as usize);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    assert!(
        !event_log_events(&h)
            .into_iter()
            .any(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
    );
    let active_head = h.agents[&cid].head.expect("active prompt head");
    let active_parent = default_agent_node(&h, active_head)
        .parent_id
        .expect("passive fact is active parent");
    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.cut == tau_proto::AgentHead::Node(active_parent)
    )));
    let submitted: Vec<(String, bool)> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptSubmitted(submitted)
                if submitted.text == passive_text || submitted.text == "real follow-up" =>
            {
                Some((submitted.text, submitted.inference_activation))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        submitted,
        vec![(passive_text, false), ("real follow-up".to_owned(), true)],
        "passive notice should commit false immediately before the active user prompt"
    );
}

#[test]
fn interception_mutating_prompt_reaches_agent() {
    // End-to-end check that mirrors the test-dummy's "Tao → Tau"
    // correction flow: an interceptor replies with
    // `Pass(Some(modified))` and the agent receives the modified
    // text in its message list. Verifies the full chain (intercept
    // request → reply with mutation → fold of mutated event →
    // c.head sync → agent dispatch with up-to-date branch) end-to-
    // end.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, "I love Tao".to_owned())
        .expect("dispatch");

    // Interceptor replies with the mutated event.
    let agent_id = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_ref())
        .expect("prompt publish assigned an agent id")
        .clone();
    let mutated = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: true,
        agent_id: crate::parse_agent_id(&agent_id),
        text: "I love Tau".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(mutated))),
        })),
    )
    .expect("intercept reply");

    // The committed user message reflects the *mutated* text — and
    // c.head points at it (see `interception_user_prompt_dispatch_
    // waits_for_commit` for the dispatch-side assertion).
    let head = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items, .. }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "I love Tau")
                )
        ),
        "the agent will see the *interceptor-mutated* text, not the user's typo"
    );
}

#[test]
fn publish_for_agent_does_not_emit_navigate_tree() {
    // Phase 4: cross-conversation publishes used to bounce
    // `tree.head()` via a `UiNavigateTree` event before folding the
    // real event. With explicit-parent folds in
    // `AgentTree::apply_event_at`, the bounce is gone — the harness
    // stamps the conversation's `head` directly.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let baseline_seq = h.event_log.next_seq();
    let cid = ensure_test_user_agent(&mut h);

    // Two prompts in a row on the same conversation. Either would
    // historically have caused `publish_for_agent_from` to
    // bounce `tree.head()` via `UiNavigateTree`.
    h.dispatch_prompt_for_agent(&cid, "first".to_owned())
        .expect("first dispatch");
    h.dispatch_prompt_for_agent(&cid, "second".to_owned())
        .expect("second dispatch");

    let mut navigates = 0;
    let mut user_msgs = 0;
    let mut id = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(id) {
        match &entry.event {
            Event::UiNavigateTree(_) => navigates += 1,
            Event::AgentPromptSubmitted(_) => user_msgs += 1,
            _ => {}
        }
        id = entry.seq.next();
    }
    assert_eq!(
        navigates, 0,
        "cross-conversation publishes must not emit UiNavigateTree anymore"
    );
    assert_eq!(user_msgs, 2);
}

#[test]
fn interception_disconnect_clears_registration() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.handle_disconnect("interceptor");
    let after_disconnect_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("not intercepted"));

    let entry = h
        .event_log
        .get_next_from(after_disconnect_seq)
        .expect("event reaches log");
    assert_eq!(entry.event, draft_event("not intercepted"));
}

#[test]
fn agent_metadata_set_and_unset_events_are_interceptable() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "metadata-interceptor");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_UNSET),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let key = tau_proto::AgentMetadataKey::new("ext_core-shell_cwd");
    let set = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: CborValue::Text("/tmp".to_owned()),
        mutation_id: Some(
            tau_proto::AgentMetadataMutationId::parse("mutation-1").expect("mutation id"),
        ),
        inheritable: true,
    });
    h.publish_event(None, set.clone());
    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, set);
    assert!(!transient, "metadata set must be durable by default");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::AgentMetadataSet(
                tau_proto::AgentMetadataSet {
                    agent_id: tau_proto::AgentId::parse("rewritten-agent").expect("agent id"),
                    key: tau_proto::AgentMetadataKey::new("rewritten-key"),
                    value: CborValue::Text("/rewritten".to_owned()),
                    mutation_id: None,
                    inheritable: false,
                },
            )))),
        })),
    )
    .expect("pass set");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentMetadataSet(committed)
            if committed.value == CborValue::Text("/rewritten".to_owned())
                && committed.agent_id == agent_id
                && committed.key == key
                && committed.inheritable
                && committed.mutation_id.as_ref().is_some_and(|id| id.as_str() == "mutation-1")
    )));

    interceptor.lock().expect("events").clear();
    let must_pass = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: CborValue::Text("/must-pass".to_owned()),
        mutation_id: Some(
            tau_proto::AgentMetadataMutationId::parse("mutation-2").expect("mutation id"),
        ),
        inheritable: true,
    });
    h.publish_event(None, must_pass.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop tokened set");
    assert!(event_log_events(&h).contains(&must_pass));

    interceptor.lock().expect("events").clear();
    let unset = Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset { agent_id, key });
    h.publish_event(None, unset.clone());
    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, unset);
    assert!(!transient, "metadata unset must be durable by default");

    h.shutdown().expect("shutdown");
}

/// Interceptors may rewrite progress payloads, but shell correlation/target
/// identity remains canonical and validated terminal delivery is immutable and
/// must-pass.
#[test]
fn shell_command_interception_preserves_identity_and_terminal_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "shell-interceptor");
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_PROGRESS),
                EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_FINISHED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("shell-agent").expect("agent id");
    let progress = Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
        command_id: "shell-progress".into(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "original".to_owned(),
        target_agent_id: Some(agent_id.clone()),
    });
    h.publish_event(None, progress.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ShellCommandProgress(
                tau_proto::ShellCommandProgress {
                    command_id: "redirected".into(),
                    stream: tau_proto::ShellStream::Stderr,
                    chunk: "rewritten".to_owned(),
                    target_agent_id: Some(
                        tau_proto::AgentId::parse("redirected-agent").expect("agent id"),
                    ),
                },
            )))),
        })),
    )
    .expect("rewrite progress");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ShellCommandProgress(committed)
            if committed.command_id.as_str() == "shell-progress"
                && committed.target_agent_id.as_ref() == Some(&agent_id)
                && committed.chunk == "rewritten"
                && committed.stream == tau_proto::ShellStream::Stderr
    )));

    interceptor.lock().expect("events").clear();
    let finished = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: "shell-finished".into(),
        session_id: "s1".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
        output: "original".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });
    h.publish_event(None, finished.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ShellCommandFinished(
                tau_proto::ShellCommandFinished {
                    command_id: "redirected".into(),
                    session_id: "other-session".into(),
                    command: "malicious".to_owned(),
                    include_in_context: true,
                    target_agent_id: Some(
                        tau_proto::AgentId::parse("redirected-agent").expect("agent id"),
                    ),
                    output: "rewritten".to_owned(),
                    exit_code: Some(7),
                    cancelled: true,
                },
            )))),
        })),
    )
    .expect("rewrite terminal");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ShellCommandFinished(committed)
            if committed.command_id.as_str() == "shell-finished"
                && committed.session_id.as_str() == "s1"
                && committed.command == "pwd"
                && !committed.include_in_context
                && committed.target_agent_id.as_ref() == Some(&agent_id)
                && committed.output == "original"
                && committed.exit_code == Some(0)
                && !committed.cancelled
    )));

    interceptor.lock().expect("events").clear();
    let must_pass = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: "shell-must-pass".into(),
        session_id: "s1".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id),
        output: "must pass".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });
    h.publish_event(None, must_pass.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop terminal");
    assert!(event_log_events(&h).contains(&must_pass));
}

/// A UI shell id remains reserved while its immutable terminal is parked in
/// interception, then becomes reusable only after that terminal commits.
#[test]
fn shell_command_ui_id_reservation_extends_through_terminal_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "shell-terminal-interceptor");
    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SHELL_COMMAND_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let ui = connect_test_client(&mut h, "shell-terminal-ui", tau_proto::ClientKind::Ui);
    h.bus
        .set_subscriptions(
            "shell-terminal-ui",
            Vec::new(),
            vec![EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND)],
        )
        .expect("subscribe ui");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agents[&cid]
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    let command = tau_proto::UiShellCommand {
        session_id: h.current_session_id.clone(),
        command_id: "parked-ui-id".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
    };
    h.handle_ui_shell_command("ui", command.clone());
    let provider_id = super::super::ui_shell_provider_ids(&h.registry)
        .into_iter()
        .next()
        .expect("shell provider");
    let first_route = h
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("first route")
        .clone();
    let terminal = tau_proto::ShellCommandFinished {
        command_id: first_route.as_protocol_id().clone(),
        session_id: command.session_id.clone(),
        command: command.command.clone(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
        output: "first".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    h.handle_extension_shell_event(provider_id.as_str(), Event::ShellCommandFinished(terminal));
    let _ = intercepted_payload(&interceptor);
    assert!(h.pending_ui_shell_commands.is_empty());
    assert!(h.active_ui_shell_command_ids.contains(&command.command_id));

    h.handle_ui_shell_command("ui", command.clone());
    assert!(h.pending_ui_shell_commands.is_empty());
    assert_eq!(
        ui.lock()
            .expect("ui sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(projected))
                    if projected.command_id == command.command_id
            ))
            .count(),
        1,
        "parked terminal keeps same-id reuse from reaching the UI"
    );

    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("must-pass terminal");
    assert!(!h.active_ui_shell_command_ids.contains(&command.command_id));

    h.handle_ui_shell_command("ui", command.clone());
    assert_eq!(h.pending_ui_shell_commands.len(), 1);
    assert_eq!(
        ui.lock()
            .expect("ui sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(projected))
                    if projected.command_id == command.command_id
            ))
            .count(),
        2
    );

    let second_route = h
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("second route")
        .clone();
    h.handle_extension_shell_event(
        provider_id.as_str(),
        Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
            command_id: second_route.as_protocol_id().clone(),
            session_id: command.session_id,
            command: command.command,
            include_in_context: false,
            target_agent_id: Some(agent_id),
            output: "second".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        }),
    );
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second terminal");
    assert!(
        !h.active_ui_shell_command_ids
            .contains(&tau_proto::ShellCommandId::new("parked-ui-id"))
    );
}

#[test]
fn invalid_metadata_interceptor_replacements_fall_back_to_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "metadata-rewriter");
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let original = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("valid"),
        value: CborValue::Text("ok".to_owned()),
        mutation_id: None,
        inheritable: true,
    });
    h.publish_event(None, original.clone());
    let replacement = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id,
        key: tau_proto::AgentMetadataKey::new("too-large"),
        value: CborValue::Bytes(vec![0; tau_proto::MAX_AGENT_METADATA_VALUE_BYTES + 1]),
        mutation_id: None,
        inheritable: true,
    });
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("replace with invalid metadata");

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| event == &original));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "too-large"
    )));

    h.shutdown().expect("shutdown");
}
