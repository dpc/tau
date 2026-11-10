use super::*;

/// Append through the canonical reducer and retain exactly the facts needed for
/// cold reconstruction, not an independent presentation cache.
fn append(
    tree: &mut tau_core::AgentTree,
    records: &mut Vec<tau_core::PersistedAgentEvent>,
    event: Event,
) {
    let record = tau_core::PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
        seq: tree.next_event_seq(),
        source: None,
        event,
        parent: tau_core::AgentEventParent::InheritHead,
        fold_semantics: tau_core::AgentJournalFoldSemantics::CommitOrder,
        recorded_at: tau_proto::UnixMicros::default(),
    };
    tree.apply_persisted_record(&record).expect("live append");
    records.push(record);
}

/// Build an external fact with independently controlled publisher and endpoint.
fn external(id: &str, publisher: &str, scope: &str, text: &str) -> Event {
    let mut message = tau_proto::MessageDelivered::new(
        publisher.parse().expect("publisher"),
        tau_proto::MessageAgentTarget::new("main"),
        tau_proto::MessageFactId::new(id),
        tau_proto::MessageParty {
            stable_id: id.to_owned(),
            display_name: None,
            sender_auth: None,
            sender_trust: Some(tau_proto::MessageSenderTrust::Untrusted),
        },
        None,
        format!("remote-body-{id}"),
    );
    message.incoming_notice = Some(
        tau_proto::MessageIncomingNotice::new(scope.to_owned(), text.to_owned()).expect("notice"),
    );
    Event::MessageDelivered(message)
}

/// Build an inter-session receive whose sender notice must remain per-message.
fn peer(id: &str, text: &str) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        peer_review: None,
        message_id: id.parse().expect("message"),
        sender_id: id.parse().expect("sender"),
        sender_session_id: Some(id.parse().expect("session")),
        cli_origin: None,
        recipient_id: crate::parse_agent_id("main"),
        kind: tau_proto::AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        sender_notice: Some(
            "remote-advice"
                .to_owned()
                .try_into()
                .expect("sender notice"),
        ),
        recipient_notice: Some(text.to_owned().try_into().expect("recipient notice")),
        message: format!("peer-body-{id}"),
    })
}

/// Compare provider-visible output and byte measurements against a cold journal
/// replay at every boundary where eligibility can change.
fn rendered(tree: &tau_core::AgentTree, records: &[tau_core::PersistedAgentEvent]) -> String {
    let cold = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), records)
        .expect("cold replay");
    let context = assemble_conversation_from(tree, tree.head());
    assert_eq!(context, assemble_conversation_from(&cold, cold.head()));
    assert_preflight_matches_materialization(tree, true);
    serde_json::to_string(&context).expect("rendered context")
}

/// Local guidance is once per configured scope, not per remote sender or text;
/// live projection, cold replay and retained post-compaction suffixes agree.
#[test]
fn incoming_notices_share_context_scoping_and_rearm_after_compaction() {
    let mut live = tau_core::AgentTree::from_events(crate::parse_agent_id("main"), &[]);
    let mut records = Vec::new();
    append(&mut live, &mut records, external("empty", "gccp", "a", ""));
    append(
        &mut live,
        &mut records,
        external("first", "gccp", "a", "endpoint-first"),
    );
    append(&mut live, &mut records, peer("peer-first", "session-first"));
    let cut = tau_proto::AgentHead::Node(live.head().expect("prefix"));
    append(
        &mut live,
        &mut records,
        external("second", "gccp", "a", "endpoint-later"),
    );
    append(
        &mut live,
        &mut records,
        peer("peer-second", "session-later"),
    );
    append(
        &mut live,
        &mut records,
        external("other", "gccp", "b", "endpoint-first"),
    );
    append(
        &mut live,
        &mut records,
        external("publisher", "other", "a", "endpoint-first"),
    );
    let text = rendered(&live, &records);
    assert_eq!(text.matches("endpoint-first").count(), 3);
    assert_eq!(text.matches("session-first").count(), 1);
    assert!(!text.contains("endpoint-later"));
    assert!(!text.contains("session-later"));
    assert_eq!(text.matches("remote-advice").count(), 2);
    assert_eq!(text.matches("remote-body-").count(), 5);
    assert!(!text.contains("\\n\\n</notice>"));

    // Only the prefix is compacted. Previously suppressed suffix notices become
    // the first snapshots of the new context without scanning replacement text.
    let suffix_end = tau_proto::AgentHead::Node(live.head().expect("suffix"));
    append(&mut live, &mut records, compaction_start_event(cut));
    let mut boundary = compacted_event(vec![materialized_message("opaque summary endpoint-first")]);
    let Event::AgentCompacted(compacted) = &mut boundary else {
        unreachable!()
    };
    compacted.cut = cut;
    compacted.suffix_end = suffix_end;
    append(&mut live, &mut records, boundary);
    let text = rendered(&live, &records);
    assert_eq!(text.matches("endpoint-later").count(), 1);
    assert_eq!(text.matches("session-later").count(), 1);
    assert!(!text.contains("session-first"));
    assert!(text.contains("opaque summary endpoint-first"));

    append(
        &mut live,
        &mut records,
        external("third", "gccp", "a", "endpoint-after"),
    );
    append(&mut live, &mut records, peer("peer-third", "session-after"));
    let text = rendered(&live, &records);
    assert!(!text.contains("endpoint-after"));
    assert!(!text.contains("session-after"));

    // A fresh recipient has no presentation state inherited from this one.
    let mut fresh = tau_core::AgentTree::from_events(crate::parse_agent_id("main"), &[]);
    let mut fresh_records = Vec::new();
    append(
        &mut fresh,
        &mut fresh_records,
        external("fresh", "gccp", "a", "endpoint-after"),
    );
    assert!(rendered(&fresh, &fresh_records).contains("endpoint-after"));
}

/// A complete compaction must rearm both local sources for the first new
/// delivery, even when the immutable replacement retains earlier advice.
#[test]
fn incoming_notices_rearm_for_first_new_delivery_after_full_compaction() {
    let mut live = tau_core::AgentTree::from_events(crate::parse_agent_id("main"), &[]);
    let mut records = Vec::new();
    append(
        &mut live,
        &mut records,
        external("before", "gccp", "a", "old-endpoint"),
    );
    append(&mut live, &mut records, peer("peer-before", "old-session"));
    let old_head = live.head();
    let cut = tau_proto::AgentHead::Node(old_head.expect("prefix"));
    append(&mut live, &mut records, compaction_start_event(cut));
    let mut boundary = compacted_event(vec![materialized_message(
        "retained old-endpoint old-session",
    )]);
    let Event::AgentCompacted(compacted) = &mut boundary else {
        unreachable!()
    };
    compacted.cut = cut;
    compacted.suffix_end = cut;
    append(&mut live, &mut records, boundary);
    append(
        &mut live,
        &mut records,
        external("after", "gccp", "a", "new-endpoint"),
    );
    append(&mut live, &mut records, peer("peer-after", "new-session"));
    append(
        &mut live,
        &mut records,
        external("repeat", "gccp", "a", "new-endpoint"),
    );
    append(&mut live, &mut records, peer("peer-repeat", "new-session"));
    let text = rendered(&live, &records);
    assert_eq!(text.matches("new-endpoint").count(), 1);
    assert_eq!(text.matches("new-session").count(), 1);
    assert!(text.contains("retained old-endpoint old-session"));
    assert!(!text.contains("remote-body-before"));
    // Selecting an older branch/cut reconstructs its own eligibility; the
    // current context never suppresses guidance in a historical selection.
    let old =
        serde_json::to_string(&assemble_conversation_from(&live, old_head)).expect("old context");
    assert_eq!(old.matches("old-endpoint").count(), 1);
    assert_eq!(old.matches("old-session").count(), 1);
    assert!(!old.contains("new-endpoint"));
}
