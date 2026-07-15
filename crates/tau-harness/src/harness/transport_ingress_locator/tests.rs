use tau_proto::{
    AgentId, ExtensionName, ExternalActorKind, ExternalMessageIdentity, MessageEndpoint, MessageId,
    MessageOperation, MessagePayload, SenderIdentityAssurance, SenderPolicyStatus, TextFormat,
    TransportMessageDraft,
};

use super::*;

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
                actor_kind: ExternalActorKind::Human,
            },
            conversation: None,
            operation: MessageOperation::Create {
                payload: MessagePayload::Text {
                    text: "hello".to_owned(),
                    format: TextFormat::Plain,
                },
            },
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
