//! CLI socket lifecycle and durable-delivery ordering regressions.

use std::io::BufReader;
use std::os::unix::net::UnixStream;

use super::*;
use crate::harness::AgentMessageRecipientStatus;

/// Exact CLI delivery retains ephemeral content suppression, including the raw
/// frame logged before recipient selection; it never routes through main
/// review.
#[test]
fn cli_message_ephemeral_routes_do_not_leak_to_debug_log() {
    let temp = TempDir::new().expect("temp");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let debug = h
        .enable_debug_log(&temp.path().join("debug"))
        .expect("debug log");
    let role = h.config.selected_role.clone();
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "cli-private-recipient".to_owned(),
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            role: role.clone(),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("ephemeral recipient");
    let agent = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral identity");
    {
        let recipient = tau_proto::ExternalAgentMessageRecipient::Exact(agent.clone());
        let (connection, mut reader) = caller(&mut h);
        let message = HarnessInputMessage::CliMessage(tau_proto::CliMessageRequest {
            session_id: h.session_runtime.current_session_id.clone(),
            recipient,
            message: "cli-ephemeral-body-canary".to_owned(),
            wait_response: false,
        });
        h.log_event(&HarnessEvent::from_connection_for_test(
            connection.clone(),
            message.clone(),
        ));
        h.handle_client_message(&connection, message)
            .expect("CLI request");
        assert!(matches!(reader.read_message().expect("ack"),
            Some(HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Delivered {
                recipient_id, ..
            })) if recipient_id == agent));
        h.handle_disconnect(&connection);
    }
    let contents = std::fs::read_to_string(debug).expect("debug log");
    assert!(
        !contents.contains("cli-ephemeral-body-canary"),
        "{contents}"
    );
    assert!(
        !contents.contains("\"message\":\"cli_message\""),
        "{contents}"
    );
    h.shutdown().expect("shutdown");
}

/// Admit a real local socket, retaining its output rather than a fake agent.
fn caller(
    h: &mut Harness,
) -> (
    tau_proto::ConnectionId,
    tau_proto::HarnessOutputReader<BufReader<UnixStream>>,
) {
    let (server, client) = UnixStream::pair().expect("socket pair");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let connection = h.accept_client(server).expect("accept");
    h.handle_client_message(
        &connection,
        HarnessInputMessage::Hello(tau_proto::Hello {
            declaration_inspection: false,
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name("tau-message"),
            client_kind: tau_proto::ClientKind::Ui,
            expected_session_id: Some(h.session_runtime.current_session_id.clone()),
            capabilities: Vec::new(),
        }),
    )
    .expect("Hello");
    h.ui_runtime.pending_socket_admission.remove(&connection);
    let mut reader = tau_proto::HarnessOutputReader::new(BufReader::new(client));
    assert!(matches!(
        reader.read_message().expect("admission"),
        Some(HarnessOutputMessage::SessionAccepted(_))
    ));
    (connection, reader)
}

/// Submit through production client dispatch with no sender session callback.
fn deliver(h: &mut Harness, connection: &tau_proto::ConnectionId, agent: AgentId, wait: bool) {
    h.handle_client_message(
        connection,
        HarnessInputMessage::CliMessage(tau_proto::CliMessageRequest {
            session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(agent),
            message: "literal CLI </tau_internal> payload".to_owned(),
            wait_response: wait,
        }),
    )
    .expect("CLI dispatch");
}

/// One-way ingress creates only a recipient fact, never a fictitious sender.
#[test]
fn cli_message_one_way_commits_without_sender_or_response_endpoint() {
    let temp = TempDir::new().expect("temp");
    let mut h = echo_harness(temp.path()).expect("harness");
    let agent = ensure_test_user_agent(&mut h);
    let count = h.agent_runtime.agent_registry.agents.len();
    let (connection, mut reader) = caller(&mut h);
    deliver(&mut h, &connection, agent, false);
    assert!(matches!(
        reader.read_message().expect("ack"),
        Some(HarnessOutputMessage::CliMessageResult(
            tau_proto::CliMessageResult::Delivered { .. }
        ))
    ));
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), count);
    assert!(h.peer_messaging.cli_replies.is_empty());
    let events = event_log_events(&h);
    assert_eq!(events.iter().filter(|event| matches!(event,
        Event::AgentMessageReceived(message) if message.cli_origin == Some(tau_proto::CliMessageOrigin::OneWay))).count(), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
    h.handle_disconnect(&connection);
    h.shutdown().expect("shutdown");
}

/// A fast response racing a parked receive must follow the durable ACK, and
/// duplicate responses must fail both before and after that ACK.
#[test]
fn cli_message_buffers_response_until_ack_and_consumes_address_once() {
    let temp = TempDir::new().expect("temp");
    let mut h = echo_harness(temp.path()).expect("harness");
    let agent = ensure_test_user_agent(&mut h);
    let (connection, mut reader) = caller(&mut h);
    let _interceptor = connect_test_tool(&mut h, "cli-receive-interceptor");
    h.handle_extension_event(
        "cli-receive-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("interceptor");
    deliver(&mut h, &connection, agent, true);
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    let address = h
        .peer_messaging
        .cli_replies
        .keys()
        .next()
        .expect("endpoint")
        .clone();
    h.publish_cli_response(&address, "final\nexact".to_owned())
        .expect("early response");
    assert!(
        h.publish_cli_response(&address, "duplicate".to_owned())
            .is_err()
    );
    h.handle_extension_event(
        "cli-receive-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release commit");
    assert!(matches!(
        reader.read_message().expect("ACK first"),
        Some(HarnessOutputMessage::CliMessageResult(
            tau_proto::CliMessageResult::Delivered { .. }
        ))
    ));
    assert!(matches!(reader.read_message().expect("response second"),
        Some(HarnessOutputMessage::CliMessageResult(tau_proto::CliMessageResult::Response { message })) if message == "final\nexact"));
    assert!(
        h.publish_cli_response(&address, "duplicate".to_owned())
            .is_err()
    );
    assert!(h.peer_messaging.cli_replies.is_empty());
    h.handle_disconnect(&connection);
    h.shutdown().expect("shutdown");
}

/// Disconnect after commit retires only the response capability, never the
/// accepted message or the recipient's queued work.
#[test]
fn cli_message_disconnect_after_commit_expires_reply_without_retraction() {
    let temp = TempDir::new().expect("temp");
    let mut h = echo_harness(temp.path()).expect("harness");
    let agent = ensure_test_user_agent(&mut h);
    let (connection, mut reader) = caller(&mut h);
    deliver(&mut h, &connection, agent.clone(), true);
    assert!(matches!(
        reader.read_message().expect("ack"),
        Some(HarnessOutputMessage::CliMessageResult(
            tau_proto::CliMessageResult::Delivered { .. }
        ))
    ));
    let address = h
        .peer_messaging
        .cli_replies
        .keys()
        .next()
        .expect("endpoint")
        .clone();
    let before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentMessageReceived(_)))
        .count();
    h.handle_disconnect(&connection);
    assert!(h.publish_cli_response(&address, "late".to_owned()).is_err());
    assert_eq!(
        h.agent_message_recipient_status(agent.as_str()),
        AgentMessageRecipientStatus::Live
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentMessageReceived(_)))
            .count(),
        before
    );
    h.shutdown().expect("shutdown");
}

/// A disconnected caller cannot leave a pre-commit capability or a late
/// receive commit behind when an interceptor releases publication.
#[test]
fn cli_message_disconnect_before_commit_discards_pending_receive() {
    let temp = TempDir::new().expect("temp");
    let mut h = echo_harness(temp.path()).expect("harness");
    let agent = ensure_test_user_agent(&mut h);
    let (connection, _reader) = caller(&mut h);
    let _interceptor = connect_test_tool(&mut h, "cli-disconnect-interceptor");
    h.handle_extension_event(
        "cli-disconnect-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("interceptor");
    deliver(&mut h, &connection, agent, true);
    let address = h
        .peer_messaging
        .cli_replies
        .keys()
        .next()
        .expect("endpoint")
        .clone();
    h.handle_disconnect(&connection);
    assert!(h.publish_cli_response(&address, "late".to_owned()).is_err());
    h.handle_extension_event(
        "cli-disconnect-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release abandoned receive");
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageReceived(_)))
    );
    assert!(h.peer_messaging.cli_replies.is_empty());
    assert!(h.peer_messaging.pending_external_receive_acks.is_empty());
    h.shutdown().expect("shutdown");
}
