use std::{sync as path_std_sync, time as path_std_time};

use super::*;
use crate::journal_sync::SyncTargetKind;
use crate::record_log::AppendFault;

fn facts_budget(max_record_bytes: u64, remaining_bytes: u64) -> AgentCreationFactsBudget {
    AgentCreationFactsBudget {
        max_record_bytes,
        remaining_bytes,
    }
}

/// Overlapping V1 owners fail before either the journal or cached tree mutates.
#[test]
fn overlapping_v1_owner_append_rejects_before_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("v1-overlap").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("durable store");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                inference_activation: true,
                message_class: Default::default(),
            }),
        )
        .expect("seed head");
    let checkpoint = |prompt: &str| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: tau_proto::AgentPromptId::parse(prompt).expect("prompt id"),
            through: tau_proto::AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
            output_length_continuation: None,
        })
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-first"),
            UnixMicros::new(1),
        )
        .expect("first owner");
    let before = store.agent_events(agent_id.as_str()).expect("events");
    let before_tree = store.agent(agent_id.as_str()).expect("tree").clone();
    let journal = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_bytes = fs::read(&journal).expect("journal bytes");
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-second"),
            UnixMicros::new(2),
        )
        .expect_err("overlap must reject");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    assert_eq!(store.agent(agent_id.as_str()).expect("tree"), &before_tree);
    assert_eq!(fs::read(&journal).expect("journal bytes"), before_bytes);
    drop(store);
    let mut reopened = AgentStore::open_lazy(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("clean reopen");
    assert_eq!(
        reopened.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    let mut overlapping_replay = before.clone();
    overlapping_replay.push(PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([9; 16]),
        seq: PersistedAgentEventSeq::new(2),
        source: None,
        event: checkpoint("ap-replay-overlap"),
        parent: AgentEventParent::Under(NodeId::new(0)),
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: UnixMicros::new(3),
    });
    assert!(
        AgentTree::try_from_events(agent_id, &overlapping_replay).is_err(),
        "cold replay must reject overlapping marked owners"
    );
}

/// A store-wide memory-only policy keeps unexpected agent activity replayable
/// without consulting or creating its supplied durable root.
#[test]
fn memory_only_store_never_touches_durable_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("state/agents");
    let seeded_agent_dir = agents_dir.join("seeded-agent");
    fs::create_dir_all(&seeded_agent_dir).expect("seed durable agent root");
    let seeded_journal = seeded_agent_dir.join("events.cbor");
    fs::write(&seeded_journal, b"seeded durable bytes").expect("seed durable journal");
    let mut store = AgentStore::open_memory_only(&agents_dir);
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    assert!(!store.agent_id_is_reserved(agent_id.as_str()));
    assert!(!store.agent_id_is_reserved("seeded-agent"));
    assert!(
        store
            .lock_and_recover_agent("seeded-agent")
            .expect("memory-only recovery is process-local")
            .is_none()
    );

    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends in memory");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "preview"),
        )
        .expect("projection appends in memory");
    store
        .record_agent_meta(agent_id.as_str())
        .expect("metadata remains in memory");

    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("memory replay")
            .len(),
        2
    );
    assert_eq!(
        store.agent(&agent_id).and_then(AgentTree::display_name),
        Some("preview")
    );
    assert_eq!(
        fs::read(&seeded_journal).expect("seed remains readable"),
        b"seeded durable bytes"
    );
    assert!(!seeded_agent_dir.join("lock").exists());
    assert!(!agents_dir.join(agent_id.as_str()).exists());
}

/// Ensures the writer rejects a record length that the matching loader would
/// reject, before opening or mutating the journal.
#[test]
fn write_record_limit_matches_read_limit() {
    let error = validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES + 1)
        .expect_err("oversized record must be rejected");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
}

/// A later semantic append and fold complete while prior journal sync remains
/// blocked in the background.
#[test]
fn semantic_append_continues_while_sync_is_blocked() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let sync = store.framed_appends.inject_blocking_sync();
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends");
    assert!(sync.wait_until_blocked(), "worker did not block");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let continued_id = agent_id.clone();
    let continuation = std::thread::spawn(move || {
        let result = store.append_agent_event(
            continued_id.as_str(),
            None,
            display_name_event(&continued_id, "continued"),
        );
        tx.send((store, result)).expect("send continuation");
    });
    let received = rx.recv_timeout(path_std_time::Duration::from_secs(2));
    sync.release();
    let (store, result) = received.expect("later semantic append blocked on sync");
    result.expect("later semantic append completes");
    continuation.join().expect("continuation thread");
    assert_eq!(
        store.agent(&agent_id).and_then(AgentTree::display_name),
        Some("continued")
    );
}

/// A later writable lifetime re-covers both an existing store root and its
/// locked branch as independent typed boundary targets.
#[test]
fn writable_reopen_recovers_store_root_and_branch_boundaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("state/agents");
    {
        let created = missing_directories(&agents_dir);
        fs::create_dir_all(&agents_dir).expect("first lifetime creates root");
        let mut first = FramedAppendState::default();
        first.inject_sync_spawn_failure();
        first.note_created_directories(created);
        assert!(first.dirty_target(&agents_dir).is_some());
    }
    let mut store = AgentStore::open_lazy(&agents_dir).expect("second store opens");
    store.framed_appends.inject_sync_spawn_failure();
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("writable append");

    let root_target = store
        .framed_appends
        .dirty_target(&agents_dir)
        .expect("store-root target");
    assert!(root_target.directories.contains(&temp.path().join("state")));
    assert!(root_target.directories.contains(&temp.path().to_path_buf()));
    assert_eq!(root_target.kind, SyncTargetKind::DirectoryBoundary);
    let branch = agents_dir.join(agent_id.as_str());
    let branch_target = store
        .framed_appends
        .dirty_target(&branch)
        .expect("branch target");
    assert_eq!(
        branch_target.directories,
        [agents_dir].into_iter().collect()
    );
    assert_eq!(branch_target.kind, SyncTargetKind::DirectoryBoundary);
}

/// The roster enrichment path reads immutable creation fields and the latest
/// in-memory display projection without scanning transcript history.
#[test]
fn creation_facts_read_valid_first_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: Some(AgentId::parse("parent").expect("parent id")),
                role: "engineer".to_owned(),
                display_name: Some("Initial".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "Latest".to_owned(),
            }),
            UnixMicros::new(43),
        )
        .expect("name appends");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("within budget");

    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            started_at: Some(started_at),
            role,
            display_name: Some(display_name),
            ..
        } if started_at == UnixMicros::new(42)
            && role == "engineer"
            && display_name == "Latest"
    ));
}

/// Cold roster enrichment accepts a journal-bound checkpoint but suppresses a
/// structurally valid checkpoint whose boundary witness no longer matches.
#[test]
fn creation_facts_require_journal_bound_cold_checkpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    agent_id: agent_id.clone(),
                    parent_agent: None,
                    role: "engineer".to_owned(),
                    display_name: Some("Cold".to_owned()),
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
                UnixMicros::new(42),
            )
            .expect("creation appends");
    }

    let store = AgentStore::open_lazy(temp.path()).expect("cold store opens");
    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("fresh checkpoint");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: Some(display_name),
            ..
        } if display_name == "Cold"
    ));

    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("invalid checkpoint stays categorical");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: None,
            ..
        }
    ));
}

/// Missing journals remain categorical rows rather than becoming I/O errors.
#[test]
fn creation_facts_report_missing_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("missing").expect("agent id");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("missing does not consume budget");

    assert_eq!(facts, AgentCreationFacts::Missing);
    assert_eq!(facts.bytes_read(), 0);
}

/// Aggregate roster enrichment fails before allocating a first record that does
/// not fit the caller's remaining budget.
#[test]
fn creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    let result = store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1));

    assert_eq!(result, Err(AgentCreationFactsBudgetExceeded));
}

/// In-memory ephemeral creation projections consume the same aggregate budget
/// even though no journal read is needed.
#[test]
fn ephemeral_creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("ephemeral").expect("agent id");
    store
        .mark_agent_ephemeral(agent_id.as_str())
        .expect("mark ephemeral");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: true,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    assert_eq!(
        store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1)),
        Err(AgentCreationFactsBudgetExceeded)
    );
}

/// Truncated durable records charge their advertised bounded length so repeated
/// malformed members cannot bypass the aggregate roster budget.
#[test]
fn truncated_creation_records_consume_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let record_length = 256 * 1024_u64;
    let mut remaining = 4 * 1024 * 1024_u64;
    for index in 0..17 {
        let agent_id = AgentId::parse(format!("truncated-{index}")).expect("agent id");
        let dir = store.agent_dir(agent_id.as_str());
        fs::create_dir_all(&dir).expect("agent dir");
        let mut bytes = record_length.to_le_bytes().to_vec();
        bytes.resize(8 + record_length as usize - 1, 0);
        fs::write(dir.join("events.cbor"), bytes).expect("truncated journal");

        let facts = store.agent_creation_facts(&agent_id, facts_budget(record_length, remaining));
        if index < 16 {
            let facts = facts.expect("record fits remaining budget");
            assert_eq!(
                facts,
                AgentCreationFacts::Unreadable {
                    bytes_read: record_length
                }
            );
            remaining = remaining.saturating_sub(facts.bytes_read());
        } else {
            assert_eq!(facts, Err(AgentCreationFactsBudgetExceeded));
        }
    }
}

/// A durable agent append failure rolls back the frame, leaves its checkpoint
/// and folded sequence unchanged, and reuses that sequence on retry.
#[test]
fn failed_frame_append_is_atomic_and_retry_reuses_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let checkpoint_before = fs::read(&checkpoint_path).expect("baseline checkpoint");
    store.framed_appends.inject_fault(
        &journal_path,
        AppendFault {
            fail_write_at: Some(3),
            ..AppendFault::default()
        },
    );

    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");

    assert!(matches!(error, AgentStoreError::Write { .. }));
    assert_eq!(fs::read(&journal_path).expect("journal"), journal_before);
    assert_eq!(
        fs::read(&checkpoint_path).expect("checkpoint"),
        checkpoint_before
    );
    assert_eq!(
        store
            .load_agent(agent_id.as_str())
            .expect("agent remains loadable")
            .expect("agent remains loaded")
            .display_name(),
        None
    );
    let retry = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "retry"),
            UnixMicros::new(43),
        )
        .expect("retry appends");
    assert_eq!(retry.seq, PersistedAgentEventSeq::new(1));
    assert_eq!(
        store
            .load_agent(agent_id.as_str())
            .expect("agent remains loadable")
            .expect("agent remains loaded")
            .display_name(),
        Some("retry")
    );
    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("valid journal")
            .len(),
        2
    );
}

/// An uncertain agent-journal rollback poisons only that live stream and later
/// appends reject it without changing its bytes.
#[test]
fn rollback_failure_poisons_agent_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    store.framed_appends.inject_fault(
        &journal_path,
        AppendFault {
            fail_write_at: Some(3),
            fail_truncate: true,
            ..AppendFault::default()
        },
    );
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");
    let bytes_after_failure = fs::read(&journal_path).expect("failed journal");

    let poisoned = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "rejected"),
            UnixMicros::new(43),
        )
        .expect_err("poisoned journal rejects append");
    let other_agent_id = AgentId::parse("agent-2").expect("agent id");
    let other = store
        .append_agent_event_at(
            other_agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&other_agent_id),
            UnixMicros::new(44),
        )
        .expect("other journal remains writable");

    assert!(
        poisoned
            .to_string()
            .contains("append disabled after an incomplete rollback")
    );
    assert_eq!(other.seq, PersistedAgentEventSeq::new(0));
    assert_eq!(
        fs::read(&journal_path).expect("poisoned journal"),
        bytes_after_failure
    );
}

/// Read-only replay rejects a partial payload at EOF, while the next locked
/// append keeps the valid prefix and removes only that incomplete crash tail.
#[test]
fn strict_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let suffix = [5_u64.to_le_bytes().as_slice(), &[1, 2]].concat();
    OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal")
        .write_all(&suffix)
        .expect("append malformed suffix");

    let error = AgentStore::open(temp.path()).expect_err("strict replay rejects torn frame");

    assert!(matches!(error, AgentStoreError::Read { .. }));
    let mut store = AgentStore::open_lazy(temp.path()).expect("lazy store opens");
    let appended = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "recovered"),
            UnixMicros::new(43),
        )
        .expect("locked append repairs suffix");
    assert_eq!(appended.seq, PersistedAgentEventSeq::new(1));
    let events = store
        .agent_events(agent_id.as_str())
        .expect("journal reads");
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1].event,
        Event::AgentDisplayNameSet(name) if name.display_name == "recovered"
    ));
}

/// A complete framed record with a malformed durable controlled identifier is
/// a decode failure, not a repairable incomplete crash tail.
#[test]
fn strict_replay_rejects_framed_record_with_malformed_agent_message_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let valid = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1_u8; 16]),
        seq: PersistedAgentEventSeq::new(1),
        source: None,
        event: Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("message-1").expect("message id"),
            sender_id: agent_id.clone(),
            recipient: tau_proto::AgentMessageRecipient::Agent {
                agent_id: AgentId::parse("recipient-agent").expect("agent id"),
            },
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello".to_owned(),
        }),
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: UnixMicros::new(42),
    };
    let mut malformed = serde_json::to_value(valid).expect("serialize record value");
    *malformed
        .pointer_mut("/event/payload/message_id")
        .expect("message id field") = serde_json::Value::String("bad.message".to_owned());
    let mut encoded = Vec::new();
    ciborium::into_writer(&malformed, &mut encoded).expect("encode malformed framed record");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write frame length");
    file.write_all(&encoded).expect("write complete frame");
    drop(file);
    let bytes_before = fs::read(&journal_path).expect("read malformed journal");

    let error = AgentStore::open(temp.path()).expect_err("malformed identifier must fail replay");

    assert!(matches!(error, AgentStoreError::Decode { .. }));
    let mut lazy = AgentStore::open_lazy(temp.path()).expect("lazy store opens");
    let append_error = lazy
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "must-not-append"),
            UnixMicros::new(43),
        )
        .expect_err("locked writer must reject complete malformed frame");
    assert!(matches!(append_error, AgentStoreError::Read { .. }));
    assert_eq!(
        fs::read(&journal_path).expect("read unchanged malformed journal"),
        bytes_before
    );
}

/// Builds the immutable first event used by durable append tests.
fn started_event(agent_id: &AgentId) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id.clone(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

/// Builds one sequence-advancing event used by durable append tests.
fn display_name_event(agent_id: &AgentId, display_name: &str) -> Event {
    Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
        agent_id: agent_id.clone(),
        display_name: display_name.to_owned(),
    })
}

/// A read-only snapshot of a missing durable agent must fail without creating
/// the requested agent directory or lock file.
#[test]
fn journal_snapshot_does_not_create_missing_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-missing").expect("agent id");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect_err("missing journal must fail");

    assert!(matches!(error, AgentStoreError::JournalMissing { .. }));
    assert!(!temp.path().join(agent_id.as_str()).exists());
}

/// A lock-held journal must expose its last checkpointed committed prefix
/// without waiting for the writer to exit or including later appends.
#[test]
fn journal_snapshot_reads_lock_held_committed_prefix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");

    let snapshot = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect("checkpointed committed prefix");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "after-cut"),
        )
        .expect("writer remains appendable");

    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("snapshot records")
            .collect::<Result<Vec<_>, _>>()
            .expect("valid committed prefix")
            .len(),
        1
    );
}

/// A lock-held journal must reject a checkpoint whose boundary witness no
/// longer authenticates the selected committed prefix.
#[test]
fn journal_snapshot_rejects_lock_held_checkpoint_boundary_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-boundary").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect_err("mismatched live checkpoint must fail");

    assert!(matches!(error, AgentStoreError::Read { .. }));
}

/// A structurally valid lock-held checkpoint must not select a prefix whose
/// decoded record count disagrees with its advertised next sequence.
#[test]
fn journal_snapshot_rejects_lock_held_checkpoint_sequence_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-sequence").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.next_seq += 1;
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect_err("wrong live checkpoint sequence must fail");

    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));
}

/// Inactive capture must acquire the shared lock before selecting EOF, so
/// an append and writer release immediately before acquisition are included.
#[test]
fn journal_snapshot_selects_inactive_eof_after_lock_acquisition() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-race").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let journal_path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_append = fs::metadata(&journal_path).expect("journal metadata").len();
    let mut store = Some(store);

    let snapshot = AgentJournalSnapshot::capture_for_test(temp.path(), [agent_id.clone()], |_| {
        store
            .as_mut()
            .expect("writer")
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(&agent_id, "before-lock"),
            )
            .expect("append before snapshot lock");
        drop(store.take());
    })
    .expect("inactive snapshot after writer release");

    assert!(
        before_append
            < fs::metadata(&journal_path)
                .expect("updated journal metadata")
                .len()
    );
    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("snapshot records")
            .collect::<Result<Vec<_>, _>>()
            .expect("complete locked EOF")
            .len(),
        2
    );
}

/// Snapshot validation must reject a journal whose stored sequence no longer
/// matches its authoritative file position.
#[test]
fn journal_snapshot_rejects_non_monotonic_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-corrupt").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let mut bytes = fs::read(&path).expect("journal");
    let seq = bytes
        .windows(5)
        .position(|window| window == b"\x63seq\x00")
        .map(|offset| offset + 4)
        .expect("sequence encoding");
    bytes[seq] = 1;
    fs::write(&path, bytes).expect("corrupt sequence");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect_err("corrupt journal must fail");

    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));
}

/// Snapshot validation must reject a crash-torn frame rather than exporting a
/// valid prefix as though it were the complete journal.
#[test]
fn journal_snapshot_rejects_torn_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-torn").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal")
        .write_all(&[1, 2, 3])
        .expect("torn header");

    let error =
        AgentJournalSnapshot::capture(temp.path(), [agent_id]).expect_err("torn journal must fail");

    assert!(matches!(error, AgentStoreError::Read { .. }));
}

/// A captured snapshot retains all journal locks, preventing records from
/// changing until the consumer has finished with the validated data.
#[test]
fn journal_snapshot_prevents_changes_during_read() {
    let temp = tempfile::tempdir().expect("tempdir");
    let first = AgentId::parse("agent-a").expect("agent id");
    let second = AgentId::parse("agent-b").expect("agent id");
    for agent_id in [&second, &first] {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
            .expect("creation");
    }
    let snapshot = AgentJournalSnapshot::capture(temp.path(), [second.clone(), first.clone()])
        .expect("stable snapshot");
    assert_eq!(
        snapshot.agent_ids().collect::<Vec<_>>(),
        vec![&first, &second],
        "snapshot identities use lexical agent order"
    );
    let absent = AgentId::parse("agent-absent").expect("agent id");
    assert!(matches!(
        snapshot.records(&absent),
        Err(AgentStoreError::JournalNotIncluded { agent_id }) if agent_id == absent
    ));

    let mut writer = AgentStore::open_lazy(temp.path()).expect("writer");
    let error = writer
        .append_agent_event(first.as_str(), None, display_name_event(&first, "changed"))
        .expect_err("snapshot lock must prevent append");
    assert!(matches!(error, AgentStoreError::Locked { .. }));
    assert_eq!(
        snapshot
            .records(&first)
            .expect("stable records")
            .collect::<Result<Vec<_>, _>>()
            .expect("records")
            .len(),
        1
    );
}
