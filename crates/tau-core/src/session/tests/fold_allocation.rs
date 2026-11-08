//! Regression coverage for state-dependent allocation admission and cold
//! replay.
//!
//! These are requested-allocation oracles, not RSS tests.

use super::*;
use crate::semantic_persistence::fold_memory::{serialization_scratch, validation_scratch};

/// One canonical event trace retained so each allocation scenario can be
/// replayed.
struct AllocationTrace {
    /// Live projection built only through validated durable appends.
    tree: AgentTree,
    /// Exact accepted records used for the cold-replay oracle.
    records: Vec<PersistedAgentEvent>,
}

impl AllocationTrace {
    /// Starts with no accepted records or transcript nodes.
    fn new() -> Self {
        Self {
            tree: AgentTree::from_events(agent_id(), &[]),
            records: Vec::new(),
        }
    }

    /// Constructs the next record without changing its candidate tree.
    fn candidate(&self, event: Event, parent: AgentEventParent) -> PersistedAgentEvent {
        PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0; 16]),
            seq: self.tree.next_event_seq(),
            source: None,
            event,
            parent,
            fold_semantics: AgentJournalFoldSemantics::CommitOrder,
            recorded_at: UnixMicros::new(self.tree.next_event_seq().get()),
        }
    }

    /// Accepts a candidate through the same validation and fold used by replay.
    fn append(&mut self, record: PersistedAgentEvent) -> Option<NodeId> {
        let node = self
            .tree
            .apply_persisted_record(&record)
            .expect("allocation fixture must be a valid durable trace");
        self.records.push(record);
        node
    }

    /// Checks that accounting did not change the accepted event's replay
    /// meaning.
    fn assert_cold_equivalent(&self) {
        let cold = AgentTree::try_from_events(agent_id(), &self.records).expect("cold trace");
        assert_eq!(self.tree, cold);
    }
}

/// Produces a simple durable input with no transport or provenance allocations.
fn allocation_input(text: String) -> Event {
    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        discovery_context: None,
        agent_id: agent_id(),
        text,
        inference_activation: false,
        message_class: Default::default(),
    })
}

/// Opens one marked inference and queues inputs behind its exact branch owner.
fn deferred_allocation_trace(count: usize, text_bytes: usize) -> AllocationTrace {
    let mut trace = AllocationTrace::new();
    let root = trace.candidate(allocation_input("root".to_owned()), AgentEventParent::Root);
    let head = trace.append(root).expect("root node");
    let mut owner = trace.candidate(
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id(),
            transaction_id: None,
            agent_prompt_id: "ap-allocation-deferred".parse().expect("prompt"),
            through: AgentHead::Node(head),
            model: "provider/model".into(),
            operation: tau_proto::PromptOperation::Inference,
            activation_cut: AgentHead::Root,
            output_length_continuation: None,
        }),
        AgentEventParent::Under(head),
    );
    owner.fold_semantics = AgentJournalFoldSemantics::InferenceDeferredInputV1;
    assert!(trace.append(owner).is_none());
    for _ in 0..count {
        let input = trace.candidate(
            allocation_input("q".repeat(text_bytes)),
            AgentEventParent::Under(head),
        );
        assert!(trace.append(input).is_none(), "input must remain deferred");
    }
    assert_eq!(trace.tree.nodes().len(), 1);
    trace
}

/// Produces the same small terminal regardless of the queued input payloads.
fn deferred_terminal(trace: &AllocationTrace) -> PersistedAgentEvent {
    trace.candidate(
        Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
            automatic_compaction_decision: None,
            agent_id: agent_id(),
            agent_prompt_id: "ap-allocation-deferred".parse().expect("prompt"),
            reason: tau_proto::AgentPromptTerminationReason::Canceled,
            originator: PromptOriginator::User,
        }),
        AgentEventParent::Under(NodeId::new(0)),
    )
}

/// A tiny terminal can materialize many queued entries. Admission must charge
/// their partition/sort/index work without charging another copy of moved text.
#[test]
fn deferred_terminal_charge_tracks_materialization_not_queued_payload_bytes() {
    let few = deferred_allocation_trace(2, 1);
    let mut many = deferred_allocation_trace(128, 1);
    let mut large_text = deferred_allocation_trace(128, 4096);
    let few_terminal = deferred_terminal(&few);
    let many_terminal = deferred_terminal(&many);
    let large_terminal = deferred_terminal(&large_text);
    assert_eq!(few_terminal.event, many_terminal.event);
    assert_eq!(many_terminal.event, large_terminal.event);
    let few_charge = few.tree.fold_allocation_charge(&few_terminal);
    let many_charge = many.tree.fold_allocation_charge(&many_terminal);
    let large_charge = large_text.tree.fold_allocation_charge(&large_terminal);
    assert!(
        many_charge >= few_charge + 126 * size_of::<AgentNode>(),
        "the small terminal needs new node/index storage for the queued inputs"
    );
    assert_eq!(
        many_charge, large_charge,
        "moving queued boxes must not deep-charge half a MiB of existing text"
    );
    for (trace, terminal, text_bytes) in [
        (&mut many, many_terminal, 1),
        (&mut large_text, large_terminal, 4096),
    ] {
        assert_eq!(trace.append(terminal), Some(NodeId::new(128)));
        assert_eq!(trace.tree.nodes().len(), 129);
        for (offset, node) in trace.tree.nodes().iter().skip(1).enumerate() {
            assert_eq!(node.parent_id, Some(NodeId::new(offset as u64)));
            let AgentEntry::UserInput { items, .. } = &node.entry else {
                panic!("deferred input must materialize as a user node");
            };
            assert!(matches!(&items[..],
                [ContextItem::Message(MessageItem { content, .. })]
                    if matches!(&content[..], [ContentPart::Text { text }]
                        if text.len() == text_bytes && text.bytes().all(|byte| byte == b'q'))
            ));
        }
        trace.assert_cold_equivalent();
    }
}

/// Creates a final tool-result fact; a text payload is normalized into both raw
/// and provider-facing retained output before the round closes.
fn allocation_tool_result(call_id: &str, result: tau_proto::CborValue) -> Event {
    Event::ProviderToolResult(tau_proto::ToolResult {
        captured_output: None,
        presentation: Default::default(),
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        result,
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    })
}

/// Leaves one potentially large result retained behind two unfinished calls.
fn tool_allocation_trace(old_bytes: usize) -> AllocationTrace {
    let mut trace = AllocationTrace::new();
    let response = trace.candidate(
        Event::ProviderResponseFinished(tool_calling_response(
            &agent_id(),
            "ap-allocation-tools",
            vec![
                ToolCallId::from("allocation-first"),
                ToolCallId::from("allocation-middle"),
                ToolCallId::from("allocation-last"),
            ],
        )),
        AgentEventParent::Root,
    );
    assert_eq!(trace.append(response), Some(NodeId::new(0)));
    let first = trace.candidate(
        allocation_tool_result(
            "allocation-first",
            tau_proto::CborValue::Text("r".repeat(old_bytes)),
        ),
        AgentEventParent::InheritHead,
    );
    assert!(trace.append(first).is_none());
    for _ in 0..16 {
        let input = trace.candidate(
            allocation_input("after tools".to_owned()),
            AgentEventParent::InheritHead,
        );
        assert!(trace.append(input).is_none());
    }
    trace
}

/// Closing a tool round clones all retained results into its ordered aggregate.
/// The last tiny record must charge large historical output exactly when that
/// clone happens, while a nonclosing result must not pay for untouched output.
#[test]
fn final_tool_charge_includes_old_results_only_when_aggregate_materializes() {
    const OLD_BYTES: usize = 2 * 1024 * 1024;
    let mut small = tool_allocation_trace(1);
    let mut large = tool_allocation_trace(OLD_BYTES);
    let middle_event = allocation_tool_result("allocation-middle", tau_proto::CborValue::Null);
    let small_middle = small.candidate(middle_event.clone(), AgentEventParent::InheritHead);
    let large_middle = large.candidate(middle_event, AgentEventParent::InheritHead);
    assert_eq!(
        small.tree.fold_allocation_charge(&small_middle),
        large.tree.fold_allocation_charge(&large_middle),
        "nonclosing insertions do not clone the old result"
    );
    assert!(small.append(small_middle).is_none());
    assert!(large.append(large_middle).is_none());

    let last_event = allocation_tool_result("allocation-last", tau_proto::CborValue::Null);
    let small_last = small.candidate(last_event.clone(), AgentEventParent::InheritHead);
    let large_last = large.candidate(last_event, AgentEventParent::InheritHead);
    let mut encoded = Vec::new();
    ciborium::into_writer(&large_last, &mut encoded).expect("encode small last record");
    assert!(encoded.len() < 4096);
    assert_eq!(small_last, large_last);
    let small_charge = small.tree.fold_allocation_charge(&small_last);
    let large_charge = large.tree.fold_allocation_charge(&large_last);
    assert!(
        large_charge >= small_charge + OLD_BYTES - 1,
        "the final record duplicates historical result payloads larger than itself"
    );

    assert_eq!(large.append(large_last), Some(NodeId::new(17)));
    assert_eq!(large.tree.nodes().len(), 18);
    let AgentEntry::ToolResults { items } =
        &large.tree.node(NodeId::new(1)).expect("aggregate").entry
    else {
        panic!("round closes with one ordered aggregate result");
    };
    assert_eq!(
        items
            .iter()
            .map(|item| item.call_id.as_str())
            .collect::<Vec<_>>(),
        ["allocation-first", "allocation-middle", "allocation-last"]
    );
    assert_eq!(
        items[0].output.raw,
        tau_proto::CborValue::Text("r".repeat(OLD_BYTES))
    );
    assert_eq!(items[0].output.body.len(), OLD_BYTES);
    for (offset, node) in large.tree.nodes().iter().skip(2).enumerate() {
        assert_eq!(node.parent_id, Some(NodeId::new(offset as u64 + 1)));
        assert!(matches!(node.entry, AgentEntry::UserInput { .. }));
    }
    large.assert_cold_equivalent();
}

/// Embeds an opaque provider item in the easily missed failed-compaction
/// carrier.
fn incomplete_failure(started: &tau_proto::AgentStandaloneCompactionStarted, raw: String) -> Event {
    Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
        agent_id: agent_id(),
        transaction_id: started.transaction_id.clone(),
        cut: started.cut,
        reason: tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded,
        resume_through: started.resume_through,
        context_retreat: None,
        output_length_continuation: None,
        incomplete_response: Some(Box::new(tau_proto::StandaloneCompactionIncomplete {
            agent_prompt_id: started.compact_prompt_id.clone(),
            output_items: vec![ContextItem::Reasoning(
                tau_proto::OpaqueProviderItem::from_raw_json(raw).expect("opaque reasoning"),
            )],
            usage: None,
            provider_response_id: Some("incomplete-allocation".to_owned()),
            provider_attempt: tau_proto::ProviderAttempt::ONE,
            backend: tau_proto::ProviderBackend {
                kind: tau_proto::ProviderBackendKind::PublicResponses,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            },
        })),
    })
}

/// Raw duplicate-key values allocate while parsing even though the retained
/// opaque value discards them. Preflight must see that structural work before
/// ContextItem's serialization reparses the incomplete-response raw JSON.
#[test]
fn incomplete_opaque_preflight_counts_structure_discarded_by_duplicate_keys() {
    const ELEMENTS: usize = 8192;
    let started = compaction_start("ct-allocation-incomplete");
    let structural_payload = format!("[{}null]", "[],".repeat(ELEMENTS));
    let flat_payload = format!("\"{}\"", "x".repeat(structural_payload.len() - 2));
    assert_eq!(structural_payload.len(), flat_payload.len());
    let structural_raw =
        format!(r#"{{"type":"reasoning","payload":{structural_payload},"payload":null}}"#);
    let flat_raw = format!(r#"{{"type":"reasoning","payload":{flat_payload},"payload":null}}"#);
    assert_eq!(structural_raw.len(), flat_raw.len());
    let structural = incomplete_failure(&started, structural_raw.clone());
    let flat = incomplete_failure(&started, flat_raw);
    let opaque = |event: &Event| {
        let Event::AgentStandaloneCompactionFailed(failed) = event else {
            panic!("failure fixture");
        };
        let ContextItem::Reasoning(opaque) = &failed
            .incomplete_response
            .as_ref()
            .expect("incomplete")
            .output_items[0]
        else {
            panic!("reasoning fixture");
        };
        opaque.value().clone()
    };
    assert_eq!(
        opaque(&structural),
        opaque(&flat),
        "retained opaque shape intentionally loses the overwritten allocation"
    );
    let flat_charge = validation_scratch(&flat);
    let structural_charge = validation_scratch(&structural);
    assert!(
        serialization_scratch(&structural)
            >= serialization_scratch(&flat) + ELEMENTS * size_of::<serde_json::Value>(),
        "the initial size-counting pass also reserves overwritten parser structure"
    );
    assert!(
        structural_charge >= flat_charge + ELEMENTS * size_of::<serde_json::Value>(),
        "equal raw byte counts and retained values cannot hide parser container work"
    );
    assert_ne!(structural_charge, usize::MAX);

    let mut trace = AllocationTrace::new();
    let start = trace.candidate(
        Event::AgentStandaloneCompactionStarted(started),
        AgentEventParent::InheritHead,
    );
    assert!(trace.append(start).is_none());
    let failure = trace.candidate(structural, AgentEventParent::InheritHead);
    trace
        .tree
        .validate_persisted_event(&failure)
        .expect("valid incomplete response");
    let mut encoded = Vec::new();
    ciborium::into_writer(&failure, &mut encoded).expect("opaque validation serializer");
    let decoded: PersistedAgentEvent =
        ciborium::from_reader(encoded.as_slice()).expect("decode incomplete response");
    assert_eq!(
        failure, decoded,
        "opaque raw JSON survives the persistence codec"
    );
    assert!(trace.append(decoded).is_none());
    trace.assert_cold_equivalent();
}

/// Opaque carriers bypass the generic event walk to avoid unreserved JSON
/// parsing. Their usage models and per-model map keys still need formatting
/// scratch, including the nested incomplete-response carrier.
#[test]
fn opaque_carrier_usage_models_reserve_formatting_before_size_counting() {
    let model_text = format!("provider/{}", "m".repeat(256 * 1024));
    let model: tau_proto::ModelId = model_text.clone().into();
    for map_key in [false, true] {
        let mut usage = tau_proto::ProviderTokenUsage::default();
        if map_key {
            usage
                .stats
                .by_model
                .insert(model.clone(), Default::default());
        } else {
            usage.model = Some(model.clone());
        }
        let mut response = tool_calling_response(&agent_id(), "ap-model-scratch", vec![]);
        response.usage = Some(usage.clone());
        let reported = Event::ProviderResponseFinishedReported(response.clone().into());
        let started = compaction_start("ct-model-scratch");
        let mut incomplete = incomplete_failure(&started, r#"{"type":"reasoning"}"#.to_owned());
        let Event::AgentStandaloneCompactionFailed(failed) = &mut incomplete else {
            unreachable!("failure fixture");
        };
        failed
            .incomplete_response
            .as_mut()
            .expect("incomplete")
            .usage = Some(usage);
        for event in [
            Event::ProviderResponseFinished(response),
            reported,
            incomplete,
        ] {
            assert!(
                serialization_scratch(&event) >= 64 * 1024 + 4 * model_text.len(),
                "{} must reserve nested usage formatting (map key: {map_key})",
                event.name(),
            );
            let mut encoded = Vec::new();
            ciborium::into_writer(&event, &mut encoded).expect("encode usage carrier");
            let decoded: Event =
                ciborium::from_reader(encoded.as_slice()).expect("decode usage carrier");
            assert_eq!(
                decoded, event,
                "formatting accounting preserves wire meaning"
            );
        }
    }
}
