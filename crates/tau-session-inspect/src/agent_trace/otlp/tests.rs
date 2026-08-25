use std::collections::BTreeSet;

use opentelemetry_proto::tonic::common::v1::any_value;
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
        fold_semantics: tau_core::AgentJournalFoldSemantics::Legacy,
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
        presentation: Default::default(),
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
        automatic_compaction_decision: None,
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
        message_id: tau_proto::AgentMessageId::parse(format!("message-{id}"))
            .expect("test message id must satisfy its identifier grammar"),
        sender_id: AgentId::parse("agent-test").expect("agent id"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: AgentId::parse("agent-peer").expect("agent id"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: payload.to_owned(),
    })
}

/// Equal text in prompt and tool ID domains must produce separate operations,
/// typed ID attributes, and collision-free production span IDs.
#[test]
fn typed_operation_keys_do_not_merge_id_domains() {
    let records = [
        record(0, 1, tool_started("shared")),
        record(1, 2, prompt_started("shared")),
        record(2, 3, tool_result("shared")),
        record(3, 4, prompt_terminated("shared")),
    ];
    let mut operations = BTreeMap::new();
    let mut endpoints = EndpointStore::new().expect("endpoint staging");
    for record in &records {
        correlate_record(&mut operations, &mut endpoints, record).expect("correlate operation");
    }

    assert_eq!(operations.len(), 2);
    assert!(operations.iter().all(|(_, state)| {
        state.start.is_some() && state.terminal.is_some() && !state.decreasing
    }));
    let agent_id = AgentId::parse("agent-test").expect("agent id");
    let spans = operations
        .iter()
        .map(|(key, state)| {
            operation_span(&[0; 16], &[0; 8], &agent_id, &mut endpoints, key, state)
                .expect("production span")
        })
        .collect::<Vec<_>>();

    assert_ne!(spans[0].span_id, spans[1].span_id);
    let attributes = spans
        .iter()
        .map(|span| {
            span.attributes
                .iter()
                .filter_map(|attribute| {
                    let any_value::Value::StringValue(value) =
                        attribute.value.as_ref()?.value.as_ref()?
                    else {
                        return None;
                    };
                    Some((attribute.key.as_str(), value.as_str()))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .collect::<Vec<_>>();
    assert!(attributes.iter().any(|attributes| {
        attributes.get("tau.tool.call_id") == Some(&"shared")
            && attributes.get("tau.operation.id") == Some(&"shared")
    }));
    assert!(attributes.iter().any(|attributes| {
        attributes.get("tau.agent.prompt_id") == Some(&"shared")
            && attributes.get("tau.operation.id") == Some(&"shared")
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

/// Many large standalone operations retain only compact IDs and endpoint
/// offsets, never their payloads.
#[test]
fn completed_large_standalone_operations_are_staged_not_retained() {
    let payload = "x".repeat(64 * 1024);
    let (empty_operations, empty_endpoints) = correlate_standalone_messages("");
    let (operations, endpoints) = correlate_standalone_messages(&payload);

    assert_eq!(
        operations.len(),
        64,
        "one staged operation remains addressable by each durable message ID"
    );
    assert_eq!(
        operation_state_retained_bytes(&operations),
        operation_state_retained_bytes(&empty_operations),
        "changing 64 message bodies must not grow retained operation state"
    );
    assert_eq!(
        endpoint_store_bytes(&endpoints),
        unique_staged_endpoint_bytes(&operations),
        "every endpoint-store byte must belong to one live operation endpoint"
    );
    assert!(
        endpoint_store_bytes(&endpoints) - endpoint_store_bytes(&empty_endpoints)
            >= 64 * payload.len(),
        "anonymous endpoint staging, not operation state, retains every message body"
    );
}

/// Correlates a fixed set of standalone messages, varying only their bodies.
fn correlate_standalone_messages(
    payload: &str,
) -> (BTreeMap<OperationKey, OperationState>, EndpointStore) {
    let mut operations = BTreeMap::new();
    let mut endpoints = EndpointStore::new().expect("endpoint staging");
    for index in 0..64 {
        correlate_record(
            &mut operations,
            &mut endpoints,
            &record(index, index + 1, message_sent(index as usize, payload)),
        )
        .expect("correlate message");
    }
    (operations, endpoints)
}

/// Counts every byte retained directly by an operation key and its compact
/// endpoint fields. The exhaustive pattern makes a new heap-owning state field
/// update this accounting instead of silently allowing message-body retention.
fn operation_state_retained_bytes(operations: &BTreeMap<OperationKey, OperationState>) -> usize {
    operations
        .iter()
        .map(|(key, state)| {
            let OperationState {
                first,
                start,
                terminal,
                previous_time,
                decreasing,
            } = state;
            operation_key_retained_bytes(key)
                + endpoint_retained_bytes(first)
                + start.as_ref().map_or(0, endpoint_retained_bytes)
                + terminal.as_ref().map_or(0, endpoint_retained_bytes)
                + std::mem::size_of_val(previous_time)
                + std::mem::size_of_val(decreasing)
        })
        .sum()
}

/// Counts the inline offsets that identify one staged endpoint without
/// retaining any event body in operation state.
fn endpoint_retained_bytes(endpoint: &Endpoint) -> usize {
    let Endpoint { offset, length } = endpoint;
    std::mem::size_of_val(offset) + std::mem::size_of_val(length)
}

/// Counts heap-backed durable operation-ID text and the inline discriminant for
/// each operation-key domain.
fn operation_key_retained_bytes(key: &OperationKey) -> usize {
    match key {
        OperationKey::OuterTurn(id) => std::mem::size_of_val(id) + id.to_string().len(),
        OperationKey::Prompt(id) => std::mem::size_of_val(id) + id.as_str().len(),
        OperationKey::Tool(id) => std::mem::size_of_val(id) + id.as_str().len(),
        OperationKey::Message(id) => std::mem::size_of_val(id) + id.to_string().len(),
        OperationKey::Compaction(id) => std::mem::size_of_val(id) + id.to_string().len(),
        OperationKey::CompactionRequest(id) => std::mem::size_of_val(id) + id.to_string().len(),
    }
}

/// Returns the exact number of bytes currently persisted in anonymous endpoint
/// staging rather than retained by correlation state.
fn endpoint_store_bytes(endpoints: &EndpointStore) -> usize {
    usize::try_from(
        endpoints
            .file
            .metadata()
            .expect("endpoint staging metadata")
            .len(),
    )
    .expect("test endpoint store fits usize")
}

/// Sums each distinct live endpoint's encoded byte length, so the test can
/// prove anonymous staging contains exactly the operation-referenced payloads.
fn unique_staged_endpoint_bytes(operations: &BTreeMap<OperationKey, OperationState>) -> usize {
    let mut offsets = BTreeSet::new();
    let mut bytes = 0;
    for state in operations.values() {
        let OperationState {
            first,
            start,
            terminal,
            previous_time: _,
            decreasing: _,
        } = state;
        for endpoint in std::iter::once(*first)
            .chain(start.iter().copied())
            .chain(terminal.iter().copied())
        {
            if offsets.insert(endpoint.offset) {
                bytes += usize::try_from(endpoint.length).expect("test endpoint length fits usize");
            }
        }
    }
    bytes
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
