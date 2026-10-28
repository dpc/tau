use tau_proto::{
    CborValue, MessageAgentTarget, MessageDelivered, MessageFactId, MessageParty, PromptOriginator,
    SessionAgentLoaded, ToolCallId, ToolName, ToolRequest, ToolStarted, ToolType,
};

use super::*;

/// Normal-build inspection state rejects lock/repair mutation without
/// artifacts.
#[test]
fn read_only_session_store_rejects_recovery_without_mutation() {
    let temp = tempfile::tempdir().expect("temporary root");
    let root = temp.path().join("sessions");
    let mut store = SessionStore::read_only(&root);
    let error = store
        .lock_and_load_existing_session("missing-session")
        .expect_err("read-only recovery rejects");
    assert!(error.to_string().contains("unavailable"));
    assert!(!root.exists());
}

/// Resume admission must fail without recreating any path when the selected
/// persisted session disappeared before its writer lock was acquired.
#[test]
fn lock_existing_session_rejects_deleted_target_without_recreation() {
    let temp = tempfile::tempdir().expect("temporary directory");
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
        store
            .record_session_meta("session-1")
            .expect("session metadata is created");
    }
    let session_dir = temp.path().join("session-1");
    std::fs::remove_dir_all(&session_dir).expect("selected session is deleted");

    let mut store = SessionStore::open_fixture(temp.path()).expect("store reopens");
    let error = store
        .lock_and_load_existing_session("session-1")
        .expect_err("deleted session must fail");

    assert!(matches!(
        error,
        SessionStoreError::SessionNotFound { ref session_id }
            if session_id.as_str() == "session-1"
    ));
    assert!(!session_dir.exists());
}

/// Resume admission must retain the existing session lock so a cooperative

/// Builds one fold-changing durable membership fact.
fn loaded_event(session_id: &str, agent_id: &str) -> Event {
    Event::SessionAgentLoaded(SessionAgentLoaded {
        main: false,
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: SessionId::parse(session_id).expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}

/// Binding is optional, immutable and reconstructed from a retained membership
/// prefix. An ordinary creation/load orphan does not become main implicitly.
#[test]
fn main_binding_survives_restart_and_rejects_replacement_or_unload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main = AgentId::parse("main").expect("id");
    let main_load = |id: &str| {
        let Event::SessionAgentLoaded(mut loaded) = loaded_event("session-1", id) else {
            unreachable!()
        };
        loaded.main = true;
        Event::SessionAgentLoaded(loaded)
    };
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store");
        store
            .append_session_event("session-1", None, loaded_event("session-1", "orphan"))
            .expect("orphan");
        assert_eq!(
            store.session("session-1").expect("session").main_agent_id(),
            None
        );
        store
            .append_session_event("session-1", None, main_load("main"))
            .expect("bind");
        assert_eq!(
            store.session("session-1").expect("session").main_agent_id(),
            Some(&main)
        );
        assert!(
            store
                .append_session_event("session-1", None, main_load("replacement"))
                .is_err()
        );
        assert!(
            store
                .append_session_event(
                    "session-1",
                    None,
                    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                        session_id: "session-1".parse().expect("id"),
                        agent_id: main.clone()
                    },)
                )
                .is_err()
        );
    }
    let mut store = SessionStore::open_fixture(temp.path()).expect("reopen");
    let membership = store
        .lock_and_load_existing_session("session-1")
        .expect("reload")
        .expect("session");
    assert_eq!(membership.main_agent_id(), Some(&main));
    assert!(membership.contains_agent(&main));
    assert!(
        store
            .append_session_event("session-1", None, main_load("replacement"))
            .is_err()
    );
    store
        .append_session_event("session-1", None, loaded_event("session-1", "worker"))
        .expect("ordinary role reuse is independent");
}

/// Stable configured publisher provenance survives a real framed journal
/// close/reopen cycle.
#[test]
fn extension_provenance_round_trips_through_framed_session_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = PersistedEventSource::Extension(
        tau_proto::ExtensionName::parse("stable-publisher").expect("extension name"),
    );
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
        store
            .append_session_event_at(
                "session-1",
                Some(source.clone()),
                loaded_event("session-1", "agent-1"),
                UnixMicros::new(41),
            )
            .expect("append session event");
    }

    let reopened = SessionStore::open(temp.path()).expect("store reopens");
    let events = reopened
        .session_events("session-1")
        .expect("read session journal");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].source, Some(source));
}

fn append_legacy_source_frame(path: &Path, record: PersistedSessionEvent) {
    let mut value = serde_json::to_value(record).expect("serialize record value");
    value["source"] = serde_json::Value::String("legacy-source".to_owned());
    let mut encoded = Vec::new();
    ciborium::into_writer(&value, &mut encoded).expect("encode malformed source record");
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write frame length");
    file.write_all(&encoded).expect("write complete frame");
}

/// A locked ordinary-session writer rejects a complete old source shape and

/// A locked restore writer applies the same fail-closed rule to complete old
/// source shapes.
#[test]
fn locked_restore_writer_preserves_complete_invalid_source_frame() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("session-1/restore-events.cbor");
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
        store
            .append_session_restore_event_at(
                "session-1",
                None,
                restore_request("call-1"),
                UnixMicros::new(41),
            )
            .expect("baseline append");
    }
    append_legacy_source_frame(
        &path,
        PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(1),
            source: None,
            event: restore_started("call-1"),
            recorded_at: UnixMicros::new(42),
        },
    );
    let before = fs::read(&path).expect("read malformed restore journal");
    let mut lazy = SessionStore::open_fixture(temp.path()).expect("lazy store opens");

    lazy.append_session_restore_event_at(
        "session-1",
        None,
        restore_started("call-1"),
        UnixMicros::new(43),
    )
    .expect_err("complete invalid source must fail locked restore load");

    assert_eq!(fs::read(&path).expect("read unchanged journal"), before);
}

/// A later writable lifetime re-covers the complete session-store ancestor

/// Builds one valid fallback message fact.
fn delivered_message(body: &str) -> Event {
    Event::MessageDelivered(MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("missing-agent"),
        MessageFactId::new("message-1"),
        MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
            sender_trust: None,
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

/// Ordinary and restore appends must not clone visible or hidden predecessor
/// payloads. A pool smaller than the retained history still admits their actual
/// incremental work, and cold replay keeps both sequence domains intact.
#[test]
fn long_session_history_keeps_incremental_normal_and_restore_admission() {
    let temp = tempfile::tempdir().expect("tempdir");
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("seed store");
        for seq in 0..12 {
            store
                .append_session_event_at(
                    "session-1",
                    None,
                    delivered_message(&"x".repeat(1024 * 1024)),
                    UnixMicros::new(seq),
                )
                .expect("seed history");
        }
    }
    for mode in [SessionPreparationMode::Resume, SessionPreparationMode::New] {
        let owner = Arc::new(
            SemanticPersistenceOwner::new(crate::PersistenceCapacity {
                max_frames: 16,
                max_streams: 4,
                max_bytes: 16 * 1024 * 1024,
            })
            .expect("bounded owner"),
        );
        let mut store = SessionStore::open_managed(temp.path(), owner).expect("managed");
        store.prepare_session("session-1", mode).expect("prepare");
        let sid = SessionId::parse("session-1").expect("session");
        let first_payload = |store: &SessionStore| {
            let projection = &store.managed[&sid];
            let records = if mode == SessionPreparationMode::New {
                &projection.hidden_predecessor_events
            } else {
                &projection.events
            };
            match &records[0].event {
                Event::MessageDelivered(message) => message.text.as_ptr(),
                _ => unreachable!("seeded message"),
            }
        };
        let before = first_payload(&store);
        let next = store.managed[&sid].membership.next_event_seq();
        store
            .append_session_event_at(
                "session-1",
                None,
                loaded_event("session-1", "new-agent"),
                UnixMicros::new(20),
            )
            .expect("normal append");
        store
            .append_session_restore_event_at(
                "session-1",
                None,
                restore_request("restored-call"),
                UnixMicros::new(21),
            )
            .expect("restore append");
        assert_eq!(first_payload(&store), before, "retained body was copied");
        assert_eq!(store.managed[&sid].membership.next_event_seq(), next.next());
        store
            .release_session("session-1", Duration::from_secs(2))
            .expect("release");
    }
    let cold = SessionStore::open(temp.path()).expect("cold");
    assert_eq!(
        cold.session_events("session-1")
            .expect("normal replay")
            .len(),
        14
    );
    assert_eq!(
        cold.session_restore_events("session-1")
            .expect("restore replay")
            .len(),
        2
    );
}

/// Builds one valid restore-stream start.
fn restore_started(call_id: &str) -> Event {
    Event::ToolStarted(ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    })
}

/// A durable session append failure leaves the fold, sequence, and metadata

/// An uncertain ordinary-journal rollback poisons only that journal; later

/// Strict session replay rejects a partial frame even when a complete valid
/// frame follows it.
#[test]
fn strict_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let journal_path;
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
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

    let error =
        SessionStore::open_fixture(temp.path()).expect_err("strict replay rejects torn frame");

    assert!(matches!(
        error,
        SessionStoreError::Read { .. } | SessionStoreError::Persistence(_)
    ));
}

/// Crash-tail journal repair preserves the canonical creation timestamp instead

/// A deterministic failure after writing the replacement temporary file leaves
/// the preceding valid manifest byte-for-byte intact.
#[test]
fn failed_manifest_replacement_preserves_previous_manifest() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("meta.json");
    let original = SessionMeta {
        created_at: 11,
        last_touched: 12,
    };
    write_meta(&path, &original).expect("write baseline manifest");
    let before = fs::read(&path).expect("read baseline manifest");
    let replacement = SessionMeta {
        created_at: 11,
        last_touched: 13,
    };

    let error = write_meta_with(&path, &replacement, |_| {
        Err(io::Error::other("injected pre-replacement failure"))
    })
    .expect_err("injected replacement failure");

    assert!(matches!(error, SessionStoreError::Write { .. }));
    assert_eq!(fs::read(&path).expect("read preserved manifest"), before);
    assert_eq!(
        fs::read_dir(temp.path())
            .expect("list manifest directory")
            .count(),
        1,
        "failed replacement must remove its temporary file"
    );
}

/// Refreshing malformed canonical state fails closed and never silently assigns
/// a new creation timestamp.
#[test]
fn malformed_manifest_refresh_preserves_invalid_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
    let path = temp.path().join("session-1/meta.json");
    fs::create_dir_all(path.parent().expect("manifest parent")).expect("create session");
    fs::write(&path, b"{not-json").expect("write malformed manifest");

    let error = store
        .record_session_meta("session-1")
        .expect_err("malformed canonical manifest must fail");

    assert!(matches!(
        error,
        SessionStoreError::Read { .. } | SessionStoreError::Persistence(_)
    ));
    assert_eq!(
        fs::read(path).expect("read malformed manifest"),
        b"{not-json"
    );
}

/// A missing manifest does not establish durable-session existence even when
/// other session artifacts remain in the directory.
#[test]
fn missing_manifest_is_not_listed_or_resumable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_dir = temp.path().join("session-1");
    fs::create_dir_all(&session_dir).expect("create orphan session directory");
    fs::write(session_dir.join("events.cbor"), []).expect("write orphan journal");

    assert!(
        list_session_metas(temp.path())
            .expect("list session manifests")
            .is_empty()
    );
    let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
    assert!(matches!(
        store
            .lock_and_load_existing_session("session-1")
            .expect_err("orphan session must not resume"),
        SessionStoreError::SessionNotFound { .. }
    ));
}

/// A restore-stream write failure leaves bytes and sequence unchanged and

/// An uncertain restore rollback poisons only the restore journal while the

/// Strict restore replay rejects a partial frame before a valid suffix.
#[test]
fn strict_restore_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let restore_path;
    {
        let mut store = SessionStore::open_fixture(temp.path()).expect("store opens");
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
    let store = SessionStore::open_fixture(temp.path()).expect("ordinary store opens");

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
