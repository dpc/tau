//! Replay owns acceptance evidence; no extension marker or provider cache does.

use super::*;

/// Construct marked input without introducing an unrelated report occurrence.
fn message() -> tau_proto::MessageDelivered {
    let mut message = tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("gccp").expect("publisher"),
        tau_proto::MessageAgentTarget::new(agent_id().as_str()),
        tau_proto::MessageFactId::new("same-id"),
        tau_proto::MessageParty {
            stable_id: "sender".to_owned(),
            display_name: None,
            sender_auth: None,
            sender_trust: None,
        },
        None,
        "accepted body",
    );
    message.idempotent_acceptance = true;
    message.incoming_notice = Some(
        tau_proto::MessageIncomingNotice::new("endpoint".to_owned(), "local advice".to_owned())
            .expect("notice"),
    );
    message
}

/// The canonical occurrence alone reconstructs dedup and conflict identity;
/// losing the journal tail deliberately removes that evidence.
#[test]
fn message_acceptance_replay_retains_identity_but_lost_tail_does_not() {
    let message = message();
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1; 16]),
        seq: PersistedAgentEventSeq::new(0),
        source: None,
        event: Event::MessageDelivered(message.clone()),
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::new(1),
    };
    let mut live = AgentTree::from_events(agent_id(), &[]);
    live.apply_persisted_record(&record).expect("accept");
    let replay =
        AgentTree::try_from_events(agent_id(), std::slice::from_ref(&record)).expect("replay");
    assert_eq!(
        live.message_acceptance_matches(&message),
        Some((true, record.seq))
    );
    assert_eq!(
        replay.message_acceptance_matches(&message),
        Some((true, record.seq))
    );
    let lost = AgentTree::try_from_events(agent_id(), &[]).expect("lost asynchronous tail");
    assert_eq!(lost.message_acceptance_matches(&message), None);
    for field in 0..7 {
        let mut changed = message.clone();
        match field {
            0 => changed.text.push_str(" changed"),
            1 => changed.sender.display_name = Some("changed".to_owned()),
            2 => changed.presentation = Some(tau_proto::MessagePresentation::Notification),
            3 => changed.sender.sender_trust = Some(tau_proto::MessageSenderTrust::Untrusted),
            4 => changed.incoming_notice = None,
            5 => {
                changed.incoming_notice = Some(
                    tau_proto::MessageIncomingNotice::new(
                        "endpoint".to_owned(),
                        "edited advice".to_owned(),
                    )
                    .expect("notice"),
                )
            }
            _ => {
                changed.incoming_notice = Some(
                    tau_proto::MessageIncomingNotice::new(
                        "different-endpoint".to_owned(),
                        "local advice".to_owned(),
                    )
                    .expect("notice"),
                )
            }
        }
        assert_eq!(
            replay.message_acceptance_matches(&changed),
            Some((false, record.seq))
        );
    }
    let mut ordinary = record.clone();
    if let Event::MessageDelivered(message) = &mut ordinary.event {
        message.idempotent_acceptance = false;
    }
    assert_eq!(
        AgentTree::from_events(agent_id(), &[ordinary]).message_acceptance_matches(&message),
        None
    );
    let mut invalid = record;
    if let Event::MessageDelivered(message) = &mut invalid.event {
        message.text.clear();
    }
    assert_eq!(
        AgentTree::from_events(agent_id(), &[invalid]).message_acceptance_matches(&message),
        None
    );
}

/// Deferred tool-round placement and provider compaction cannot erase retained
/// message identity or make a retry eligible for another activation.
#[test]
fn message_acceptance_survives_deferred_placement_and_compaction() {
    let main = agent_id();
    let message = message();
    let mut live = AgentTree::from_events(main.clone(), &[]);
    let mut records = Vec::new();
    let mut append = |tree: &mut AgentTree, event| {
        let record = PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
            seq: tree.next_event_seq(),
            source: None,
            event,
            parent: AgentEventParent::InheritHead,
            fold_semantics: AgentJournalFoldSemantics::CommitOrder,
            recorded_at: UnixMicros::default(),
        };
        tree.apply_persisted_record(&record).expect("append");
        records.push(record);
        AgentTree::try_from_events(main.clone(), &records).expect("cold replay")
    };
    let call: ToolCallId = "acceptance-tool".into();
    append(
        &mut live,
        Event::ProviderResponseFinished(tool_calling_response(
            &main,
            "ap-acceptance-tool",
            vec![call.clone()],
        )),
    );
    let seq = live.next_event_seq();
    let cold = append(&mut live, Event::MessageDelivered(message.clone()));
    assert!(live.node_for_durable_event_seq(seq).is_none());
    assert_eq!(live.message_acceptance_matches(&message), Some((true, seq)));
    assert_eq!(cold.message_acceptance_matches(&message), Some((true, seq)));
    append(
        &mut live,
        Event::ProviderToolResult(tau_proto::ToolResult {
            captured_output: None,
            presentation: Default::default(),
            call_id: call,
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            result: tau_proto::CborValue::Text("done".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
    );
    let cut = AgentHead::Node(live.head().expect("head"));
    let mut started = compaction_start("ct-acceptance");
    started.cut = cut;
    started.resume_through = Some(cut);
    append(
        &mut live,
        Event::AgentStandaloneCompactionStarted(started.clone()),
    );
    let cold = append(
        &mut live,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            effectiveness: None,
            compaction_input_bytes: None,
            original_input_tokens: None,
            compaction_output_tokens: None,
            compact_prompt_id: started.compact_prompt_id,
            model: started.model,
            operation: started.operation,
            agent_id: main.clone(),
            transaction_id: started.transaction_id,
            cut,
            suffix_end: cut,
            replacement_window: vec![ContextItem::Compaction(
                tau_proto::OpaqueProviderItem::from_raw_json(
                    r#"{"type":"compaction","encrypted_content":"replacement"}"#,
                )
                .expect("opaque"),
            )],
        }),
    );
    assert_eq!(live.message_acceptance_matches(&message), Some((true, seq)));
    assert_eq!(cold.message_acceptance_matches(&message), Some((true, seq)));
}
