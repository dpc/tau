use tau_core::{AgentEventParent, PersistedAgentEventSeq};
use tau_proto::{
    CborValue, ModelParams, PromptOperation, PromptOriginator, ToolName, ToolResultKind, ToolType,
    UnixMicros,
};

use super::*;

fn correlate(records: &[PersistedAgentEvent]) -> BTreeMap<OperationKey, OperationState> {
    let mut operations = BTreeMap::new();
    let mut endpoints = EndpointStore::new().expect("anonymous endpoint staging");
    for record in records {
        correlate_record(&mut operations, &mut endpoints, record).expect("stage endpoint");
    }
    operations
}

fn record(seq: u64, time: u64, event: Event) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        recorded_at: UnixMicros::new(time),
    }
}

fn tool_started(id: &str) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: id.into(),
        tool_name: ToolName::new("trace_tool"),
        arguments: CborValue::Bytes(vec![0, 255]),
        agent_id: AgentId::parse("agent-test").expect("agent id"),
        originator: PromptOriginator::User,
    })
}

fn tool_result(id: &str) -> Event {
    Event::ProviderToolResult(tau_proto::ToolResult {
        call_id: id.into(),
        tool_name: ToolName::new("trace_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    })
}

fn background_placeholder(id: &str) -> Event {
    let Event::ProviderToolResult(mut value) = tool_result(id) else {
        unreachable!("tool_result helper variant")
    };
    value.kind = ToolResultKind::BackgroundPlaceholder;
    Event::ProviderToolResult(value)
}

fn background_result(id: &str) -> Event {
    Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
        call_id: id.into(),
        tool_name: ToolName::new("trace_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Text("real background result".to_owned()),
        display: None,
        originator: PromptOriginator::User,
    })
}

fn prompt_started(id: &str) -> Event {
    Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_prompt_id: id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        model: "provider/model".into(),
        model_params: Some(ModelParams::default()),
        outer_turn_id: None,
        operation: PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    })
}

fn prompt_terminated(id: &str) -> Event {
    Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
        agent_id: AgentId::parse("agent-test").expect("agent id"),
        agent_prompt_id: id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        reason: tau_proto::AgentPromptTerminationReason::Canceled,
        originator: PromptOriginator::User,
    })
}

fn message_sent(id: usize, payload: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: format!("message-{id}").into(),
        sender_id: AgentId::parse("agent-test").expect("agent id"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: AgentId::parse("agent-peer").expect("agent id"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: payload.to_owned(),
    })
}

/// Equal text in prompt and tool ID domains must produce separate
/// operations, and each explicit terminal must close only its own typed
/// lifecycle.
#[test]
fn typed_operation_keys_do_not_merge_id_domains() {
    let operations = correlate(&[
        record(0, 1, tool_started("shared")),
        record(1, 2, prompt_started("shared")),
        record(2, 3, tool_result("shared")),
        record(3, 4, prompt_terminated("shared")),
    ]);

    assert_eq!(operations.len(), 2);
    assert!(operations.iter().all(|(_, state)| {
        state.start.is_some() && state.terminal.is_some() && !state.decreasing
    }));
}

/// Any timestamp regression in authoritative operation sequence makes the
/// operation incomplete even when a later terminal timestamp increases.
#[test]
fn intermediate_timestamp_regression_invalidates_duration() {
    let operations = correlate(&[
        record(0, 3, tool_started("call-1")),
        record(1, 1, tool_started("call-1")),
        record(2, 4, tool_result("call-1")),
    ]);
    let state = operations.values().next().expect("tool operation");

    assert!(state.decreasing);
    assert!(state.start.is_some());
    assert!(state.terminal.is_some());
}

/// A synthetic background placeholder is auxiliary; the later real result
/// closes the original start without creating another operation key.
#[test]
fn background_tool_placeholder_does_not_finalize_operation() {
    let records = [
        record(0, 1, tool_started("call-1")),
        record(1, 2, background_placeholder("call-1")),
        record(2, 3, background_result("call-1")),
    ];
    let operations = correlate(&records);
    let state = operations.values().next().expect("tool operation");

    assert_eq!(operations.len(), 1);
    assert!(state.start.is_some());
    assert!(state.terminal.is_some());
    let mut endpoints = EndpointStore::new().expect("endpoint staging");
    let mut direct = BTreeMap::new();
    for record in &records {
        correlate_record(&mut direct, &mut endpoints, record).expect("correlation");
    }
    let terminal = endpoints
        .load(
            direct
                .values()
                .next()
                .and_then(|state| state.terminal)
                .expect("real terminal"),
        )
        .expect("load terminal");
    assert!(matches!(
        terminal,
        StagedEndpoint::Record(record)
            if matches!(record.event, Event::ToolBackgroundResult(_))
    ));
}

/// Repeated durable message facts with one typed ID still produce one logical
/// operation and one deterministic span ID.
#[test]
fn repeated_message_key_is_one_operation() {
    let operations = correlate(&[
        record(0, 1, message_sent(7, "first")),
        record(1, 2, message_sent(7, "second")),
    ]);

    assert_eq!(operations.len(), 1);
}

/// Equal text across every CHAIN correlation domain must still generate unique
/// span-ID seeds inside one trace.
#[test]
fn chain_domains_generate_distinct_span_ids() {
    let keys = [
        OperationKey::OuterTurn("shared".to_owned().into()),
        OperationKey::Message("shared".into()),
        OperationKey::Compaction(
            tau_proto::CompactionTransactionId::parse("shared").expect("transaction id"),
        ),
        OperationKey::CompactionRequest(
            tau_proto::CompactionRequestId::parse("shared").expect("request id"),
        ),
    ];
    let ids = keys
        .iter()
        .map(|key| {
            hashed_id(
                &format!("operation:agent-test:{}:{}", key.domain_name(), key.id()),
                8,
            )
        })
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(ids.len(), keys.len());
}

/// Many large standalone operations retain only compact IDs and endpoint
/// offsets, never their payloads.
#[test]
fn completed_large_standalone_operations_are_staged_not_retained() {
    let mut operations = BTreeMap::new();
    let mut endpoints = EndpointStore::new().expect("endpoint staging");
    let payload = "x".repeat(64 * 1024);

    for index in 0..64 {
        correlate_record(
            &mut operations,
            &mut endpoints,
            &record(index, index + 1, message_sent(index as usize, &payload)),
        )
        .expect("correlate message");
    }

    assert_eq!(operations.len(), 64);
    assert!(std::mem::size_of::<OperationState>() <= 128);
}

/// Correlation handles many distinct maximum-pressure string IDs without
/// comparison-time string copies; retained heap is the documented key bytes
/// plus fixed-size endpoint state.
#[test]
fn many_unique_large_operation_ids_remain_distinct() {
    let mut operations = BTreeMap::new();
    let mut endpoints = EndpointStore::new().expect("endpoint staging");

    for index in 0..128 {
        let id = format!("call-{index:03}-{}", "x".repeat(64 * 1024));
        correlate_record(
            &mut operations,
            &mut endpoints,
            &record(index, index + 1, tool_started(&id)),
        )
        .expect("correlate large ID");
    }

    assert_eq!(operations.len(), 128);
}
