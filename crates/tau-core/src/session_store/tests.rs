use tau_proto::{
    CborValue, MessageAgentTarget, MessageDelivered, MessageFactId, MessageParty,
    MessagePublisherId, PromptOriginator, SessionAgentLoaded, ToolCallId, ToolName, ToolRequest,
    ToolStarted, ToolType,
};

use super::*;
use crate::journal_sync::SyncTargetKind;
use crate::record_log::AppendFault;

/// Builds one fold-changing durable membership fact.
fn loaded_event(session_id: &str, agent_id: &str) -> Event {
    Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

        session_id: SessionId::parse(session_id).expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}

/// A later writable lifetime re-covers the complete session-store ancestor
/// chain plus its locked session branch.
#[test]
fn writable_reopen_recovers_session_root_and_branch_boundaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path().join("state/sessions");
    {
        let created = missing_directories(&sessions_dir);
        fs::create_dir_all(&sessions_dir).expect("first lifetime creates root");
        let mut first = FramedAppendState::default();
        first.inject_sync_spawn_failure();
        first.note_created_directories(created);
        assert!(first.dirty_target(&sessions_dir).is_some());
    }
    let mut store = SessionStore::open_lazy(&sessions_dir).expect("second store opens");
    store.framed_appends.inject_sync_spawn_failure();
    store
        .append_session_event_at(
            "session-1",
            None,
            loaded_event("session-1", "agent-1"),
            UnixMicros::new(42),
        )
        .expect("writable append");

    let root_target = store
        .framed_appends
        .dirty_target(&sessions_dir)
        .expect("store-root target");
    assert!(root_target.directories.contains(&temp.path().join("state")));
    assert!(root_target.directories.contains(&temp.path().to_path_buf()));
    assert_eq!(root_target.kind, SyncTargetKind::DirectoryBoundary);
    let branch = sessions_dir.join("session-1");
    let branch_target = store
        .framed_appends
        .dirty_target(&branch)
        .expect("branch target");
    assert_eq!(
        branch_target.directories,
        [sessions_dir].into_iter().collect()
    );
    assert_eq!(branch_target.kind, SyncTargetKind::DirectoryBoundary);
}

/// Builds one valid fallback message fact.
fn delivered_message(body: &str) -> Event {
    Event::MessageDelivered(MessageDelivered::new(
        MessagePublisherId::new("bridge-main"),
        MessageAgentTarget::new("missing-agent"),
        MessageFactId::new("message-1"),
        MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        body.to_owned(),
    ))
}

/// Builds one valid restore-stream request.
fn restore_request(call_id: &str) -> Event {
    Event::ToolRequest(ToolRequest {
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("demo"),
        tool_type: ToolType::Function,
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    })
}

/// Builds one valid restore-stream start.
fn restore_started(call_id: &str) -> Event {
    Event::ToolStarted(ToolStarted {
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    })
}

/// A durable session append failure leaves the fold, sequence, and metadata
/// unchanged, then reuses its sequence on a successful retry.
#[test]
fn failed_frame_append_is_atomic_and_retry_reuses_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    store
        .append_session_event_at(
            "session-1",
            None,
            loaded_event("session-1", "baseline-agent"),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = temp.path().join("session-1/events.cbor");
    let meta_path = temp.path().join("session-1/meta.json");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let meta_before = fs::read(&meta_path).expect("baseline metadata");
    let failed_agent = AgentId::parse("failed-agent").expect("agent id");
    store.framed_appends.inject_fault(
        &journal_path,
        AppendFault {
            fail_write_at: Some(3),
            ..AppendFault::default()
        },
    );

    let error = store
        .append_session_event_at(
            "session-1",
            None,
            loaded_event("session-1", failed_agent.as_str()),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");

    assert!(matches!(error, SessionStoreError::Write { .. }));
    assert_eq!(fs::read(&journal_path).expect("journal"), journal_before);
    assert_eq!(fs::read(&meta_path).expect("metadata"), meta_before);
    assert!(
        !store
            .session("session-1")
            .expect("loaded membership")
            .contains_agent(&failed_agent)
    );
    let retry = store
        .append_session_event_at(
            "session-1",
            None,
            loaded_event("session-1", failed_agent.as_str()),
            UnixMicros::new(43),
        )
        .expect("retry appends");
    assert_eq!(retry.seq, PersistedSessionEventSeq::new(1));
    assert!(
        store
            .session("session-1")
            .expect("loaded membership")
            .contains_agent(&failed_agent)
    );
}

/// An uncertain ordinary-journal rollback poisons only that journal; later
/// appends leave it untouched while another session remains writable.
#[test]
fn rollback_failure_poisons_only_selected_session_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message("baseline"),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = temp.path().join("session-1/events.cbor");
    store.framed_appends.inject_fault(
        &journal_path,
        AppendFault {
            fail_write_at: Some(3),
            fail_truncate: true,
            ..AppendFault::default()
        },
    );
    store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message("failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");
    let bytes_after_failure = fs::read(&journal_path).expect("failed journal");

    let poisoned = store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message("rejected"),
            UnixMicros::new(43),
        )
        .expect_err("poisoned journal rejects append");
    let other = store
        .append_session_event_at(
            "session-2",
            None,
            loaded_event("session-2", "other-agent"),
            UnixMicros::new(44),
        )
        .expect("other journal remains writable");

    assert!(
        poisoned
            .to_string()
            .contains("append disabled after an incomplete rollback")
    );
    assert_eq!(other.seq, PersistedSessionEventSeq::new(0));
    assert_eq!(
        fs::read(&journal_path).expect("poisoned journal"),
        bytes_after_failure
    );
}

/// Strict session replay rejects a partial frame even when a complete valid
/// frame follows it.
#[test]
fn strict_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let journal_path;
    {
        let mut store = SessionStore::open(temp.path()).expect("store opens");
        store
            .append_session_event_at(
                "session-1",
                None,
                delivered_message("baseline"),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = temp.path().join("session-1/events.cbor");
    }
    let record = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(1),
        source: None,
        event: delivered_message("suffix"),
        recorded_at: UnixMicros::new(42),
    };
    append_partial_frame_and_valid_suffix(&journal_path, &record);

    let error = SessionStore::open(temp.path()).expect_err("strict replay rejects torn frame");

    assert!(matches!(error, SessionStoreError::Read { .. }));
}

/// Failed journal-derived metadata rebuild remains explicit debt, survives a
/// restart, and retries after the obstruction clears.
#[test]
fn repaired_session_metadata_rebuild_retries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_dir = temp.path().join("session-1");
    let journal_path = session_dir.join("events.cbor");
    {
        let mut store = SessionStore::open(temp.path()).expect("store opens");
        store
            .append_session_event_at(
                "session-1",
                None,
                delivered_message("baseline"),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
    }
    OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal")
        .write_all(&[1, 2, 3])
        .expect("append torn header");
    let meta_path = session_dir.join("meta.json");
    fs::remove_file(&meta_path).expect("remove metadata file");
    fs::create_dir(&meta_path).expect("obstruct metadata removal");

    let mut store = SessionStore::open_lazy(temp.path()).expect("store opens");
    store
        .lock_and_load_session("session-1")
        .expect("recovery succeeds");
    assert!(
        store
            .dirty_meta_rebuilds
            .contains(&SessionId::parse("session-1").expect("known-safe SessionId must be valid"))
    );
    drop(store);

    let mut store = SessionStore::open_lazy(temp.path()).expect("store restarts");
    store
        .lock_and_load_session("session-1")
        .expect("cold validation rediscovers metadata debt");
    assert!(
        store
            .dirty_meta_rebuilds
            .contains(&SessionId::parse("session-1").expect("known-safe SessionId must be valid"))
    );

    fs::remove_dir(&meta_path).expect("remove obstruction");
    store
        .lock_and_load_session("session-1")
        .expect("retry succeeds");
    assert!(store.dirty_meta_rebuilds.is_empty());
}

/// A restore-stream write failure leaves bytes and sequence unchanged and
/// successfully retries the same sequence.
#[test]
fn failed_restore_append_is_atomic_and_retry_reuses_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_request("call-1"),
            UnixMicros::new(41),
        )
        .expect("baseline restore appends");
    let restore_path = temp.path().join("session-1/restore-events.cbor");
    let restore_before = fs::read(&restore_path).expect("baseline restore journal");
    store.framed_appends.inject_fault(
        &restore_path,
        AppendFault {
            fail_write_at: Some(5),
            ..AppendFault::default()
        },
    );

    store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_started("call-1"),
            UnixMicros::new(42),
        )
        .expect_err("injected restore append fails");

    assert_eq!(
        fs::read(&restore_path).expect("restore journal"),
        restore_before
    );
    store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_started("call-1"),
            UnixMicros::new(43),
        )
        .expect("restore retry appends");
    let events = store
        .session_restore_events("session-1")
        .expect("valid restore journal");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].seq, PersistedSessionEventSeq::new(1));
}

/// An uncertain restore rollback poisons only the restore journal while the
/// ordinary session journal remains writable.
#[test]
fn rollback_failure_poisons_only_restore_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_request("call-1"),
            UnixMicros::new(41),
        )
        .expect("baseline restore appends");
    let restore_path = temp.path().join("session-1/restore-events.cbor");
    store.framed_appends.inject_fault(
        &restore_path,
        AppendFault {
            fail_write_at: Some(5),
            fail_truncate: true,
            ..AppendFault::default()
        },
    );
    store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_started("call-1"),
            UnixMicros::new(42),
        )
        .expect_err("injected restore append fails");
    let bytes_after_failure = fs::read(&restore_path).expect("failed restore journal");

    let poisoned = store
        .append_session_restore_event_at(
            "session-1",
            None,
            restore_started("call-1"),
            UnixMicros::new(43),
        )
        .expect_err("poisoned restore journal rejects append");
    let ordinary = store
        .append_session_event_at(
            "session-1",
            None,
            loaded_event("session-1", "ordinary-agent"),
            UnixMicros::new(44),
        )
        .expect("ordinary journal remains writable");

    assert!(
        poisoned
            .to_string()
            .contains("append disabled after an incomplete rollback")
    );
    assert_eq!(ordinary.seq, PersistedSessionEventSeq::new(0));
    assert_eq!(
        fs::read(&restore_path).expect("poisoned restore journal"),
        bytes_after_failure
    );
}

/// Strict restore replay rejects a partial frame before a valid suffix.
#[test]
fn strict_restore_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let restore_path;
    {
        let mut store = SessionStore::open(temp.path()).expect("store opens");
        store
            .append_session_restore_event_at(
                "session-1",
                None,
                restore_request("call-1"),
                UnixMicros::new(41),
            )
            .expect("baseline restore appends");
        restore_path = temp.path().join("session-1/restore-events.cbor");
    }
    let record = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(1),
        source: None,
        event: restore_started("call-1"),
        recorded_at: UnixMicros::new(42),
    };
    append_partial_frame_and_valid_suffix(&restore_path, &record);
    let store = SessionStore::open(temp.path()).expect("ordinary store opens");

    let error = store
        .session_restore_events("session-1")
        .expect_err("strict restore replay rejects torn frame");

    assert!(matches!(error, SessionStoreError::Read { .. }));
}

/// Appends a torn prefix followed by one complete encoded frame.
fn append_partial_frame_and_valid_suffix(path: &Path, record: &PersistedSessionEvent) {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).expect("encode suffix");
    let mut suffix = vec![1, 2, 3];
    suffix.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
    suffix.extend_from_slice(&encoded);
    OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open journal")
        .write_all(&suffix)
        .expect("append malformed suffix");
}
