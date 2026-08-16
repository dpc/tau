use std::collections as path_std_collections;

use super::*;

/// A terminal-incomplete provider snapshot is final for its exact prompt and
/// cannot be replaced by a later retry, error, or blocked observation.
#[test]
fn terminal_incomplete_watch_state_is_sticky() {
    let status = |state| tau_proto::AgentWatchProviderStatusNotification {
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        subscription_id: String::new(),
        turn_generation: 7,
        agent_prompt_id: tau_proto::AgentPromptId::parse("ap-length").expect("prompt id"),
        state,
        initial: false,
    };
    let terminal = status(tau_proto::AgentWatchProviderState::TerminalIncomplete {
        category: tau_proto::AgentWatchProviderCategory::OutputLength,
        attempt: 3,
    });
    for later in [
        tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 4,
            next_retry_delay_secs: 1,
        },
        tau_proto::AgentWatchProviderState::TerminalError {
            failure_kind: tau_proto::ProviderFailureKind::Unknown,
            attempt: 4,
        },
        tau_proto::AgentWatchProviderState::Blocked {
            category: tau_proto::AgentWatchProviderCategory::Compaction,
        },
    ] {
        assert!(provider_status_update_is_stale(&terminal, &status(later)));
    }
}

/// Peer I/O admission rejects excess work before spawning another worker and
/// releases every process-wide slot when the admitted jobs finish.
#[test]
fn peer_io_admission_is_non_queued_and_bounded() {
    let outbound = (0..MAX_OUTBOUND_PEER_IO_JOBS)
        .map(|_| PeerIoPermit::outbound().expect("outbound slot"))
        .collect::<Vec<_>>();
    assert!(PeerIoPermit::outbound().is_none());
    drop(outbound);
    assert!(PeerIoPermit::outbound().is_some());

    let connection = tau_proto::ConnectionId::parse("bounded-peer")
        .expect("test connection id must satisfy the identifier grammar");
    let inbound = (0..MAX_INBOUND_PEER_AUTH_JOBS_PER_CONNECTION)
        .map(|_| PeerIoPermit::inbound(connection.clone()).expect("inbound slot"))
        .collect::<Vec<_>>();
    assert!(PeerIoPermit::inbound(connection).is_none());
    drop(inbound);
}

/// Isolated runtime lookup leases reject the seventeenth stalled operation and
/// restore capacity after every retained worker exits.
#[test]
fn runtime_lookup_admission_is_non_queued_and_recovers() {
    let permits = (0..MAX_OUTBOUND_PEER_IO_JOBS)
        .map(|_| RuntimeLookupPermit::try_acquire().expect("runtime lookup slot"))
        .collect::<Vec<_>>();
    assert!(RuntimeLookupPermit::try_acquire().is_none());
    drop(permits);
    assert!(RuntimeLookupPermit::try_acquire().is_some());
}

#[test]
fn message_tool_schema_requires_recipient_and_message() {
    let spec = message_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    assert_eq!(
        parameters["required"],
        serde_json::json!(["recipient_id", "message"])
    );
}

#[test]
fn message_args_require_non_empty_recipient_and_message() {
    let ok = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text("agent-a".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    let parsed = parse_message_args(&ok).expect("valid message args");
    assert_eq!(parsed.recipient_id, "agent-a");
    assert_eq!(parsed.message, "hello");

    let missing = CborValue::Map(vec![(
        CborValue::Text("recipient_id".to_owned()),
        CborValue::Text("agent-a".to_owned()),
    )]);
    assert_eq!(
        parse_message_args(&missing),
        Err("`message` is required".to_owned())
    );

    let empty = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text(" ".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    assert_eq!(
        parse_message_args(&empty),
        Err("`recipient_id` must not be empty".to_owned())
    );
}

/// Ensures sent/received agent-message events can be correlated uniquely even
/// when one sender emits multiple messages with the same timestamp.
#[test]
fn generated_agent_message_ids_are_unique_for_same_sender_and_timestamp() {
    let mut seen = path_std_collections::HashSet::new();
    let sender_id = crate::parse_agent_id("agent-test");
    let timestamp = tau_proto::UnixMicros::new(42);

    for sequence in 1..=1000 {
        let id = build_agent_message_id(&sender_id, timestamp, sequence);
        assert!(seen.insert(id), "message id must be unique");
    }
}

/// The largest producer inputs remain below the validated message-ID cap and
/// preserve the complete sender/timestamp/sequence representation.
#[test]
fn generated_agent_message_id_accepts_maximum_producer_inputs() {
    let sender = crate::parse_agent_id("a".repeat(tau_proto::AGENT_ID_MAX_LEN));
    let id = build_agent_message_id(&sender, tau_proto::UnixMicros::new(u64::MAX), u64::MAX);

    assert_eq!(
        id.as_str(),
        format!("msg-{}-{}-{}", sender.as_str(), u64::MAX, u64::MAX)
    );
    assert!(id.as_str().len() <= tau_proto::SESSION_SCOPED_ID_MAX_LEN);
}
