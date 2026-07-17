use serde::Serialize;
use tau_core::{AgentEventParent, PersistedAgentEvent, PersistedAgentEventSeq};
use tau_proto::{
    AgentId, AgentMessageIncoming, CborValue, ConnectionId, Event, ExtensionName,
    ExternalActorKind, ExternalMessageIdentity, HarnessNotice, MessageContentTrust,
    MessageEndpoint, MessageEnvelope, MessageId, MessageOperation, MessagePayload,
    MessageTransportRef, MessageTrust, NoticeLevel, SenderIdentityAssurance, SenderPolicyStatus,
    TextFormat, TransportMessageDraft, UnixMicros,
};

use super::*;

/// Historical durable agent record written before per-journal sequence
/// metadata was introduced.
#[derive(Serialize)]
struct LegacyPersistedAgentEvent {
    /// Historical global event id.
    id: u64,
    /// Connection that published the event.
    source: Option<ConnectionId>,
    /// Persisted semantic event.
    event: Event,
    /// Explicit fold parent.
    parent: AgentEventParent,
    /// Original append timestamp.
    recorded_at: UnixMicros,
}

/// Historical sequenced wrapper whose unrelated event payload no longer
/// decodes under the current schema.
#[derive(Serialize)]
struct ObsoletePersistedAgentEvent {
    /// Explicit journal sequence.
    seq: PersistedAgentEventSeq,
    /// Connection that published the event.
    source: Option<ConnectionId>,
    /// Structurally typed event with an obsolete payload.
    event: ObsoleteEvent,
    /// Explicit fold parent.
    parent: AgentEventParent,
    /// Original append timestamp.
    recorded_at: UnixMicros,
}

/// Stable event discriminator paired with an intentionally obsolete payload.
#[derive(Serialize)]
struct ObsoleteEvent {
    /// Stable event wire name.
    event: &'static str,
    /// Payload that predates the current event schema.
    payload: CborValue,
}

/// Writes one length-prefixed record in the agent journal format.
fn write_agent_record(path: &Path, record: &impl Serialize) {
    let mut payload = Vec::new();
    ciborium::into_writer(record, &mut payload).expect("encode agent record");
    let mut framed = (payload.len() as u64).to_le_bytes().to_vec();
    framed.extend(payload);
    fs::write(path, framed).expect("write agent record");
}

fn key(value: &str) -> TransportDedupKey {
    TransportDedupKey {
        extension_name: ExtensionName::from("std-slack"),
        transport_name: "slack".to_owned(),
        dedup_key: value.to_owned(),
    }
}

fn record(value: &str) -> TransportDedupRecord {
    TransportDedupRecord {
        draft: TransportMessageDraft {
            transport_name: "slack".to_owned(),
            external_endpoint: MessageEndpoint::External {
                stable_id: Some("U1".to_owned()),
                display_name: None,
                identity_alias: None,
                actor_kind: ExternalActorKind::Human,
            },
            conversation: None,
            operation: MessageOperation::Create {
                payload: MessagePayload::Text {
                    text: "hello".to_owned(),
                    format: TextFormat::Plain,
                },
            },
            transport_identity_mentioned: false,
            identity_assurance: SenderIdentityAssurance::VerifiedAccount,
            policy_status: SenderPolicyStatus::Allowlisted,
            external_identity: Some(ExternalMessageIdentity {
                dedup_key: Some(value.to_owned()),
                ..ExternalMessageIdentity::default()
            }),
            ordering: None,
            occurred_at: None,
            send_tool: None,
        },
        target_agent_id: AgentId::parse("agent-a").expect("agent"),
        message_id: MessageId::new(format!("msg-{value}")),
        committed: true,
        session_id: "s1".into(),
    }
}

/// Builds the canonical incoming event represented by one locator record.
fn incoming_event(record: &TransportDedupRecord) -> Event {
    Event::AgentMessageIncoming(AgentMessageIncoming {
        recipient_id: record.target_agent_id.clone(),
        envelope: MessageEnvelope {
            message_id: record.message_id.clone(),
            transport: MessageTransportRef {
                name: record.draft.transport_name.clone(),
                instance: Some(ExtensionName::from("std-slack")),
            },
            source: record.draft.external_endpoint.clone(),
            destination: MessageEndpoint::Agent {
                session_id: Some(record.session_id.clone()),
                agent_id: record.target_agent_id.clone(),
                display_name: None,
            },
            conversation: record.draft.conversation.clone(),
            operation: record.draft.operation.clone(),
            transport_identity_mentioned: record.draft.transport_identity_mentioned,
            trust: MessageTrust {
                content: MessageContentTrust::UntrustedExternal,
                identity: record.draft.identity_assurance,
                policy: record.draft.policy_status,
            },
            external_identity: record.draft.external_identity.clone(),
            ordering: record.draft.ordering,
            occurred_at: record.draft.occurred_at,
            reply_path: None,
        },
    })
}

/// Repeated misses share one retained-history rebuild instead of scanning once
/// per key.
#[test]
fn repeated_misses_rebuild_retained_history_once() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut locator = TransportIngressLocator::new(temp.path());
    for value in ["missing-a", "missing-b", "missing-c"] {
        assert_eq!(
            locator.lookup(&store, &key(value)).expect("lookup"),
            LocatorLookup::Missing
        );
    }
    assert_eq!(locator.rebuild_count(), 1);
}

/// Legacy and unrelated schema-drift journals do not hide a canonical owner in
/// another journal or permit a retry to move that occurrence.
#[test]
fn compatibility_journals_preserve_cross_agent_canonical_owner() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let legacy_dir = temp.path().join("legacy-agent");
    fs::create_dir(&legacy_dir).expect("legacy agent dir");
    write_agent_record(
        &legacy_dir.join("events.cbor"),
        &LegacyPersistedAgentEvent {
            id: 7,
            source: None,
            event: Event::HarnessNotice(HarnessNotice {
                kind: "legacy".to_owned(),
                message: "before durable sequences".to_owned(),
                level: NoticeLevel::Info,
                always_show: false,
            }),
            parent: AgentEventParent::InheritHead,
            recorded_at: UnixMicros::new(1),
        },
    );

    let obsolete_dir = temp.path().join("obsolete-agent");
    fs::create_dir(&obsolete_dir).expect("obsolete agent dir");
    write_agent_record(
        &obsolete_dir.join("events.cbor"),
        &ObsoletePersistedAgentEvent {
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: ObsoleteEvent {
                event: "agent.head_moved",
                payload: CborValue::Map(Vec::new()),
            },
            parent: AgentEventParent::InheritHead,
            recorded_at: UnixMicros::new(2),
        },
    );

    let canonical = record("retained");
    let canonical_dir = temp.path().join(canonical.target_agent_id.as_str());
    fs::create_dir(&canonical_dir).expect("canonical agent dir");
    write_agent_record(
        &canonical_dir.join("events.cbor"),
        &PersistedAgentEvent {
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: incoming_event(&canonical),
            parent: AgentEventParent::InheritHead,
            recorded_at: UnixMicros::new(3),
        },
    );

    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    assert!(matches!(
        store.agent_events("legacy-agent"),
        Err(tau_core::AgentStoreError::Decode { .. })
    ));
    assert!(matches!(
        store.agent_events("obsolete-agent"),
        Err(tau_core::AgentStoreError::Decode { .. })
    ));
    let mut locator = TransportIngressLocator::new(temp.path());
    assert_eq!(
        locator.lookup(&store, &key("retained")).expect("rebuild"),
        LocatorLookup::Found(Box::new(canonical.clone()))
    );
    let mut conflicting_retry = canonical.clone();
    conflicting_retry.target_agent_id = AgentId::parse("other-agent").expect("conflicting agent");
    assert!(matches!(
        locator
            .reserve(&store, &key("retained"), &conflicting_retry)
            .expect("deduplicated retry"),
        LocatorReservation::Found(found) if *found == canonical
    ));
    assert_eq!(locator.rebuild_count(), 1);
}

/// Corrupt canonical history latches one fail-closed rebuild result so retries
/// cannot repeatedly scan global history.
#[test]
fn corrupt_canonical_failure_is_sticky() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let agent_dir = temp.path().join("agent-a");
    fs::create_dir(&agent_dir).expect("agent dir");
    fs::write(agent_dir.join("events.cbor"), b"bad").expect("corrupt log");
    let mut locator = TransportIngressLocator::new(temp.path());
    assert_eq!(
        locator.lookup(&store, &key("x")),
        Err(LocatorFailure::Unavailable)
    );
    assert_eq!(
        locator.lookup(&store, &key("x")),
        Err(LocatorFailure::Unavailable)
    );
    assert_eq!(locator.rebuild_count(), 1);
}

/// The agents-root lock serializes missing-key reservation across independent
/// harness locator instances and prevents overlapping dirty ownership.
#[test]
fn independent_locators_serialize_reservations() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut first = TransportIngressLocator::new(temp.path());
    let mut second = TransportIngressLocator::new(temp.path());
    assert!(matches!(
        first.reserve(&store, &key("a"), &record("a")),
        Ok(LocatorReservation::Reserved)
    ));
    assert_eq!(
        second.lookup(&store, &key("b")),
        Err(LocatorFailure::Capacity)
    );
    first.cancel_reservation();
    assert_eq!(
        second
            .lookup(&store, &key("b"))
            .expect("lookup after release"),
        LocatorLookup::Missing
    );
}

/// A syntactically valid but inconsistent head cannot authorize absence; the
/// locator rebuilds canonical history instead.
#[test]
fn valid_cbor_head_corruption_forces_rebuild() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut first = TransportIngressLocator::new(temp.path());
    assert_eq!(
        first.lookup(&store, &key("x")).expect("initial rebuild"),
        LocatorLookup::Missing
    );
    let corrupt = LocatorHead {
        version: LOCATOR_SCHEMA_VERSION,
        count: 1,
        log_bytes: 0,
        last_hash: [7; 32],
    };
    write_head_atomic(&temp.path().join(LOCATOR_HEAD), &corrupt).expect("corrupt head");
    let mut reopened = TransportIngressLocator::new(temp.path());
    assert_eq!(
        reopened.lookup(&store, &key("x")).expect("safe rebuild"),
        LocatorLookup::Missing
    );
    assert_eq!(reopened.rebuild_count(), 1);
}

/// A crash-retained dirty marker forces canonical journal rebuild and is
/// removed only after a new clean log/head pair commits.
#[test]
fn dirty_marker_forces_canonical_rebuild() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    write_and_sync(&temp.path().join(LOCATOR_DIRTY), b"dirty").expect("dirty");
    sync_parent(&temp.path().join(LOCATOR_DIRTY)).expect("sync parent");
    let mut locator = TransportIngressLocator::new(temp.path());
    assert_eq!(
        locator.lookup(&store, &key("missing")).expect("rebuild"),
        LocatorLookup::Missing
    );
    assert_eq!(locator.rebuild_count(), 1);
    assert!(!try_exists(&temp.path().join(LOCATOR_DIRTY)).expect("dirty stat"));
}

/// A successful reservation appends one record, commits its integrity head, and
/// removes dirty state before another locator observes it.
#[test]
fn reservation_commit_updates_append_log_and_head() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut locator = TransportIngressLocator::new(temp.path());
    let committed = record("committed");
    assert!(matches!(
        locator.reserve(&store, &key("committed"), &committed),
        Ok(LocatorReservation::Reserved)
    ));
    locator
        .commit(key("committed"), committed.clone())
        .expect("commit locator");
    assert!(!try_exists(&temp.path().join(LOCATOR_DIRTY)).expect("dirty stat"));
    let head = read_head(&temp.path().join(LOCATOR_HEAD)).expect("head");
    assert_eq!(head.count, 1);
    assert!(head.log_bytes > 0);
}

/// A derived-head failure after reservation leaves dirty state in place so cold
/// recovery cannot trust the old clean index.
#[test]
fn post_publication_locator_failure_retains_dirty_state() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut locator = TransportIngressLocator::new(temp.path());
    let committed = record("failure");
    assert!(matches!(
        locator.reserve(&store, &key("failure"), &committed),
        Ok(LocatorReservation::Reserved)
    ));
    fs::remove_file(temp.path().join(LOCATOR_HEAD)).expect("remove head");
    fs::create_dir(temp.path().join(LOCATOR_HEAD)).expect("block head replacement");
    assert_eq!(
        locator.commit(key("failure"), committed),
        Err(LocatorFailure::Durable)
    );
    assert!(try_exists(&temp.path().join(LOCATOR_DIRTY)).expect("dirty stat"));
}

/// Prospective per-record byte capacity rejects before creating dirty state.
#[test]
fn oversized_locator_record_rejects_before_reservation() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let mut locator = TransportIngressLocator::new(temp.path());
    let mut oversized = record("oversized");
    oversized.draft.operation = MessageOperation::Create {
        payload: MessagePayload::Text {
            text: "x".repeat(MAX_INDEX_RECORD_BYTES as usize),
            format: TextFormat::Plain,
        },
    };
    assert!(matches!(
        locator.reserve(&store, &key("oversized"), &oversized),
        Err(LocatorFailure::Capacity)
    ));
    assert!(!try_exists(&temp.path().join(LOCATOR_DIRTY)).expect("dirty stat"));
}

/// Persisted ambiguity tombstones and located records whose canonical journal
/// was pruned produce typed fail-closed lookup states.
#[test]
fn ambiguous_and_pruned_append_log_entries_fail_closed() {
    let ambiguous_root = tempfile::TempDir::new().expect("ambiguous state");
    let (bytes, head) =
        encode_log(vec![DiskEntry::Ambiguous { key: key("same") }]).expect("encode ambiguity");
    write_atomic(&ambiguous_root.path().join(LOCATOR_LOG), &bytes).expect("log");
    write_head_atomic(&ambiguous_root.path().join(LOCATOR_HEAD), &head).expect("head");
    let store = AgentStore::open_lazy(ambiguous_root.path()).expect("store");
    let mut locator = TransportIngressLocator::new(ambiguous_root.path());
    assert_eq!(
        locator.lookup(&store, &key("same")),
        Err(LocatorFailure::Ambiguous)
    );

    let pruned_root = tempfile::TempDir::new().expect("pruned state");
    let (bytes, head) = encode_log(vec![DiskEntry::Located {
        key: key("gone"),
        record: Box::new(record("gone")),
    }])
    .expect("encode located record");
    write_atomic(&pruned_root.path().join(LOCATOR_LOG), &bytes).expect("log");
    write_head_atomic(&pruned_root.path().join(LOCATOR_HEAD), &head).expect("head");
    fs::create_dir(pruned_root.path().join("agent-a")).expect("agent dir");
    fs::write(pruned_root.path().join("agent-a/events.cbor"), b"").expect("empty journal");
    let store = AgentStore::open_lazy(pruned_root.path()).expect("store");
    let mut locator = TransportIngressLocator::new(pruned_root.path());
    assert_eq!(
        locator.lookup(&store, &key("gone")),
        Err(LocatorFailure::Pruned)
    );
}

/// Count capacity includes ambiguity tombstones and rejects before dirty state.
#[test]
fn locator_count_capacity_rejects_before_reservation() {
    let temp = tempfile::TempDir::new().expect("capacity state");
    let store = AgentStore::open_lazy(temp.path()).expect("store");
    let mut locator = TransportIngressLocator::new(temp.path());
    assert_eq!(
        locator.lookup(&store, &key("missing")).expect("initialize"),
        LocatorLookup::Missing
    );
    locator.ambiguous = (0..MAX_LOCATOR_RECORDS)
        .map(|index| key(&format!("ambiguous-{index}")))
        .collect();
    assert!(matches!(
        locator.reserve(&store, &key("new"), &record("new")),
        Err(LocatorFailure::Capacity)
    ));
    assert!(!try_exists(&temp.path().join(LOCATOR_DIRTY)).expect("dirty stat"));
}
