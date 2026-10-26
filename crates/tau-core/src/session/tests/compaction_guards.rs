use super::*;

/// Builds a contiguous canonical record without changing the tree.
fn record(tree: &AgentTree, parent: AgentEventParent, event: Event) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0; 16]),
        seq: tree.next_event_seq(),
        source: None,
        event,
        parent,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::default(),
    }
}

/// Appends through durable admission and retains the exact cold-replay input.
fn append(
    tree: &mut AgentTree,
    records: &mut Vec<PersistedAgentEvent>,
    parent: AgentEventParent,
    event: Event,
) -> AgentHead {
    let record = record(tree, parent, event);
    tree.apply_persisted_record(&record).expect("valid record");
    records.push(record);
    tree.head().map_or(AgentHead::Root, AgentHead::Node)
}

/// Rejects the same malformed record live and cold without consuming sequence,
/// transcript, indexes, transaction ownership, or recovery state.
fn assert_rejected(
    live: &AgentTree,
    records: &[PersistedAgentEvent],
    parent: AgentEventParent,
    event: Event,
    diagnostic: &str,
) {
    let cold = AgentTree::try_from_events(agent_id(), records).expect("valid prefix replays");
    assert_eq!(*live, cold);
    let rejected = record(live, parent, event);
    for mut tree in [live.clone(), cold] {
        let before = tree.clone();
        let error = tree
            .apply_persisted_record(&rejected)
            .expect_err("malformed record must reject");
        assert!(error.to_string().contains(diagnostic), "{error}");
        assert_eq!(
            tree, before,
            "rejection must leave the full projection intact"
        );
    }
    let mut invalid_history = records.to_vec();
    invalid_history.push(rejected);
    assert!(AgentTree::try_from_events(agent_id(), &invalid_history).is_err());
}

/// Creates a valid replacement correlated to the supplied immutable start.
fn success(started: &tau_proto::AgentStandaloneCompactionStarted, suffix_end: AgentHead) -> Event {
    Event::AgentCompacted(tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        agent_id: agent_id(),
        transaction_id: started.transaction_id.clone(),
        cut: started.cut,
        suffix_end,
        compact_prompt_id: started.compact_prompt_id.clone(),
        model: started.model.clone(),
        operation: started.operation,
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    })
}

/// Success cannot split function, custom, or mixed rounds. Historical open-cut
/// starts/failures must still replay and permit explicit closed-prefix
/// recovery.
#[test]
fn compaction_success_rejects_open_cut_without_mutation_live_and_cold() {
    for tool_types in [
        vec![ToolType::Function],
        vec![ToolType::Custom],
        vec![ToolType::Function, ToolType::Custom],
    ] {
        let mut tree = AgentTree::from_events(agent_id(), &[]);
        let mut records = Vec::new();
        let prefix = append(
            &mut tree,
            &mut records,
            AgentEventParent::Root,
            prompt_event(agent_id()),
        );
        let calls: Vec<ToolCallId> = (0..tool_types.len())
            .map(|index| format!("call-guard-{index}").into())
            .collect();
        let mut response = tool_calling_response(&agent_id(), "ap-guard-tools", calls.clone());
        for (item, tool_type) in response.output_items.iter_mut().zip(&tool_types) {
            let ContextItem::ToolCall(call) = item else {
                panic!("tool-only response fixture");
            };
            call.tool_type = *tool_type;
        }
        let assistant = append(
            &mut tree,
            &mut records,
            AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(response),
        );
        for (call, tool_type) in calls.into_iter().zip(tool_types) {
            append(
                &mut tree,
                &mut records,
                AgentEventParent::InheritHead,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    presentation: Default::default(),
                    call_id: call,
                    tool_name: ToolName::new("tool"),
                    tool_type,
                    result: tau_proto::CborValue::Text("result".to_owned()),
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: None,
                    originator: PromptOriginator::User,
                }),
            );
        }
        let results = AgentHead::Node(tree.head().expect("whole results"));
        assert_ne!(assistant, results);
        let mut started = compaction_start("ct-open-cut");
        started.cut = assistant;
        started.resume_through = Some(results);
        append(
            &mut tree,
            &mut records,
            AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(started.clone()),
        );
        assert_rejected(
            &tree,
            &records,
            AgentEventParent::InheritHead,
            success(&started, results),
            "provider-closed cut",
        );
        append(
            &mut tree,
            &mut records,
            AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
                agent_id: agent_id(),
                transaction_id: started.transaction_id.clone(),
                cut: started.cut,
                reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
                resume_through: started.resume_through,
                context_retreat: None,
                output_length_continuation: None,
                incomplete_response: None,
            }),
        );
        assert_eq!(
            tree,
            AgentTree::try_from_events(agent_id(), &records).expect("historical failed open start")
        );
        let mut recovery = compaction_start("ct-closed-recovery");
        recovery.compact_prompt_id = "ap-closed-recovery".parse().expect("prompt id");
        recovery.cut = prefix;
        recovery.resume_through = Some(results);
        recovery.supersedes = Some(started.transaction_id);
        append(
            &mut tree,
            &mut records,
            AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(recovery.clone()),
        );
        append(
            &mut tree,
            &mut records,
            AgentEventParent::InheritHead,
            success(&recovery, results),
        );
        let window = tree.active_provider_window(tree.head());
        assert_eq!(window.replacement.expect("summary").len(), 1);
        assert_eq!(
            window
                .transcript
                .iter()
                .map(|(id, _)| AgentHead::Node(*id))
                .collect::<Vec<_>>(),
            vec![assistant, results],
            "exact intact call/result suffix"
        );
        assert!(tree.active_provider_window_contains(
            AgentHead::Node(tree.head().expect("boundary")),
            assistant
        ));
        assert!(tree.active_provider_window_contains(
            AgentHead::Node(tree.head().expect("boundary")),
            results
        ));
        assert_eq!(
            tree,
            AgentTree::try_from_events(agent_id(), &records).expect("closed recovery replays")
        );
    }
}

/// A continuation must descend from its committed replacement, not merely the
/// original cut/resume watermark. Explicit-parent ownership is not global-head
/// equality, and both the boundary and later descendants remain valid.
#[test]
fn compaction_checkpoint_requires_successful_boundary_live_and_cold() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let mut records = Vec::new();
    let suffix = append(
        &mut tree,
        &mut records,
        AgentEventParent::Root,
        prompt_event(agent_id()),
    );
    let started = compaction_start("ct-boundary-owner");
    append(
        &mut tree,
        &mut records,
        AgentEventParent::InheritHead,
        Event::AgentStandaloneCompactionStarted(started.clone()),
    );
    let boundary = append(
        &mut tree,
        &mut records,
        AgentEventParent::InheritHead,
        success(&started, suffix),
    );
    let descendant = append(
        &mut tree,
        &mut records,
        AgentEventParent::InheritHead,
        prompt_event(agent_id()),
    );
    let sibling = append(
        &mut tree,
        &mut records,
        AgentEventParent::Under(suffix.as_option().expect("suffix")),
        prompt_event(agent_id()),
    );
    let checkpoint = |through| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id(),
            transaction_id: Some(started.transaction_id.clone()),
            agent_prompt_id: "ap-boundary-continuation".parse().expect("prompt id"),
            through,
            model: started.model.clone(),
            operation: tau_proto::PromptOperation::Inference,
            activation_cut: started.cut,
            output_length_continuation: None,
        })
    };
    for through in [AgentHead::Root, suffix, sibling] {
        assert_rejected(
            &tree,
            &records,
            AgentEventParent::InheritHead,
            checkpoint(through),
            "successful compaction boundary",
        );
    }
    for through in [boundary, descendant] {
        let mut live = tree.clone();
        let mut history = records.clone();
        append(
            &mut live,
            &mut history,
            AgentEventParent::Under(through.as_option().expect("continuation head")),
            checkpoint(through),
        );
        assert_eq!(
            live,
            AgentTree::try_from_events(agent_id(), &history).expect("valid continuation replays")
        );
        assert_eq!(
            live.compaction_chain_view(&started.transaction_id)
                .expect("owned chain")
                .completion,
            crate::CompactionChainCompletion::Complete,
        );
    }
}
