use super::*;

/// A different model's accepted replacement must not strand explicit recovery,
/// clear automatic suppression, or authorize consuming the surviving suffix.
/// Validate every record through live admission and compare cold replay.
#[test]
fn cross_model_replacement_allows_only_explicit_boundary_recovery_live_and_cold() {
    fn append(tree: &mut AgentTree, records: &mut Vec<PersistedAgentEvent>, event: Event) {
        let record = PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
            seq: tree.next_event_seq(),
            source: None,
            event,
            parent: AgentEventParent::InheritHead,
            fold_semantics: AgentJournalFoldSemantics::CommitOrder,
            recorded_at: UnixMicros::new(records.len() as u64),
        };
        tree.apply_persisted_record(&record)
            .expect("durable admission");
        records.push(record);
    }

    let mut live = AgentTree::from_events(agent_id(), &[]);
    let mut records = Vec::new();
    append(&mut live, &mut records, prompt_event(agent_id()));
    let cut = AgentHead::Node(live.head().expect("input cut"));
    let mut a = compaction_start("ct-model-a-failed");
    a.model = "provider/model-a".into();
    a.compact_prompt_id = "ap-model-a-failed".parse().expect("prompt id");
    a.cut = cut;
    a.resume_through = Some(cut);
    append(
        &mut live,
        &mut records,
        Event::AgentStandaloneCompactionStarted(a.clone()),
    );
    let failed = tau_proto::AgentStandaloneCompactionFailed {
        agent_id: agent_id(),
        transaction_id: a.transaction_id.clone(),
        cut,
        reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
        resume_through: Some(cut),
        context_retreat: None,
        output_length_continuation: None,
        incomplete_response: None,
    };
    append(
        &mut live,
        &mut records,
        Event::AgentStandaloneCompactionFailed(failed.clone()),
    );
    let mut b = compaction_start("ct-model-b-success");
    b.model = "provider/model-b".into();
    b.compact_prompt_id = "ap-model-b-success".parse().expect("prompt id");
    b.cut = cut;
    b.resume_through = None;
    append(
        &mut live,
        &mut records,
        Event::AgentStandaloneCompactionStarted(b.clone()),
    );
    let replacement = vec![ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: "summary".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })];
    append(
        &mut live,
        &mut records,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            effectiveness: None,
            compaction_input_bytes: None,
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: agent_id(),
            transaction_id: b.transaction_id,
            cut,
            suffix_end: cut,
            compact_prompt_id: b.compact_prompt_id,
            model: b.model,
            operation: b.operation,
            replacement_window: replacement.clone(),
        }),
    );
    let boundary = AgentHead::Node(live.head().expect("replacement"));
    // Both replacement-only and replacement-plus-suffix windows must work.
    for with_suffix in [false, true] {
        if with_suffix {
            append(&mut live, &mut records, prompt_event(agent_id()));
        }
        let cold = AgentTree::try_from_events(agent_id(), &records).expect("cold replay");
        for tree in [&live, &cold] {
            let head = AgentHead::Node(tree.head().expect("head"));
            assert_eq!(
                tree.unresolved_standalone_compaction_failure(&a.model, head),
                Some(&failed)
            );
            assert!(!tree.automatic_compaction_retry_is_ready(&failed, head, None));
            assert_eq!(
                tree.standalone_compaction_recovery_cut(cut, head),
                Some(boundary)
            );
            let mut recovery = compaction_start("ct-model-a-recovery");
            recovery.model = a.model.clone();
            recovery.compact_prompt_id = "ap-model-a-recovery".parse().expect("prompt id");
            recovery.supersedes = Some(a.transaction_id.clone());
            recovery.resume_through = Some(head);
            recovery.cut = cut;
            assert!(
                tree.validate_event(&Event::AgentStandaloneCompactionStarted(recovery.clone()))
                    .is_err()
            );
            if with_suffix {
                recovery.cut = head;
                assert!(
                    tree.validate_event(&Event::AgentStandaloneCompactionStarted(recovery.clone()))
                        .is_err()
                );
            }
            recovery.cut = boundary;
            let mut dropped_resume = recovery.clone();
            dropped_resume.resume_through = None;
            assert!(
                tree.validate_event(&Event::AgentStandaloneCompactionStarted(dropped_resume))
                    .is_err()
            );
            let mut wrong_model = recovery.clone();
            wrong_model.model = "provider/model-b".into();
            assert!(
                tree.validate_event(&Event::AgentStandaloneCompactionStarted(wrong_model))
                    .is_err()
            );
            tree.validate_event(&Event::AgentStandaloneCompactionStarted(recovery.clone()))
                .expect("explicit replacement recovery");
            let mut recovered = tree.clone();
            let mut recovered_records = records.clone();
            append(
                &mut recovered,
                &mut recovered_records,
                Event::AgentStandaloneCompactionStarted(recovery.clone()),
            );
            assert!(
                recovered
                    .validate_event(&Event::AgentStandaloneCompactionStarted(recovery.clone()))
                    .is_err()
            );
            let compacted = Event::AgentCompacted(tau_proto::AgentCompacted {
                effectiveness: None,
                compaction_input_bytes: None,
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: agent_id(),
                transaction_id: recovery.transaction_id,
                cut: boundary,
                suffix_end: head,
                compact_prompt_id: recovery.compact_prompt_id,
                model: recovery.model,
                operation: recovery.operation,
                replacement_window: replacement.clone(),
            });
            let suffix = tree.active_provider_window(head.as_option()).transcript;
            append(&mut recovered, &mut recovered_records, compacted.clone());
            assert!(recovered.validate_event(&compacted).is_err());
            let replay = AgentTree::try_from_events(agent_id(), &recovered_records)
                .expect("recovered replay");
            for result in [&recovered, &replay] {
                let head = AgentHead::Node(result.head().expect("recovered head"));
                assert!(
                    result
                        .unresolved_standalone_compaction_failure(&a.model, head)
                        .is_none()
                );
                assert_eq!(
                    result.active_provider_window(head.as_option()).transcript,
                    suffix
                );
            }
        }
    }
}
