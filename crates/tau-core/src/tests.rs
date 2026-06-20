use std::path::PathBuf;

use tau_proto::{
    AgentDisplayNameSet, AgentHead, AgentHeadMoved, AgentId, AgentPromptSubmitted, Event,
    PromptMessageClass, PromptOriginator, SessionAgentLoaded, SessionAgentUnloaded, SessionId,
};

use crate::{
    AgentEntry, AgentEventParent, AgentStore, AgentStoreError, NodeId, PersistedAgentEvent,
    PersistedAgentEventSeq, PersistedSessionEvent, PersistedSessionEventSeq, SessionStore,
    SessionStoreError,
};

fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "tau-core-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    path
}

fn append_raw_cbor<T: serde::Serialize>(path: &std::path::Path, record: &T) {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).expect("encode test record");
    std::fs::create_dir_all(path.parent().expect("record parent")).expect("create parent");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open record stream");
    use std::io::Write;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write record length");
    file.write_all(&encoded).expect("write record body");
}

fn agent_prompt(agent_id: &str, text: &str) -> Event {
    Event::AgentPromptSubmitted(AgentPromptSubmitted {
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        text: text.to_owned(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        display_name: None,
        ctx_id: None,
    })
}

#[test]
fn agent_store_rejects_empty_display_name() {
    // Display names are user-visible labels. Blank durable updates must not
    // suppress the id fallback in UIs or extensions.
    let agents_dir = temp_dir("empty-display-name");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let error = store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                display_name: "   ".to_owned(),
            }),
        )
        .expect_err("blank display names are invalid");

    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_meta_initializes_and_explicitly_bumps_last_user_interaction() {
    // User-interaction time is metadata state, not derived from replayable
    // transcript events. Background agent events must not refresh it when old
    // agents are loaded or replayed; accepted UI prompts call the explicit bump.
    let agents_dir = temp_dir("last-user-interaction");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .record_agent_meta("agent-1")
        .expect("record initial metadata");
    let meta = store
        .agent_meta("agent-1")
        .expect("read initial agent meta")
        .expect("agent meta exists");
    assert_ne!(meta.created_at, 0);
    assert_eq!(meta.last_user_interaction_time, meta.created_at);

    let meta_path = agents_dir.join("agent-1").join("meta.json");
    std::fs::write(
        &meta_path,
        r#"{
  "created_at": 1,
  "last_touched": 1,
  "last_user_interaction_time": 1
}"#,
    )
    .expect("seed deterministic metadata");

    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                display_name: "Research".to_owned(),
            }),
        )
        .expect("append display-name event");
    let meta = store
        .agent_meta("agent-1")
        .expect("read meta after background event")
        .expect("agent meta exists");
    assert_eq!(meta.last_user_interaction_time, 1);

    store
        .record_agent_user_interaction("agent-1")
        .expect("record explicit user interaction");
    let meta = store
        .agent_meta("agent-1")
        .expect("read meta after user interaction")
        .expect("agent meta exists");
    assert!(meta.last_user_interaction_time > 1);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_persists_transcript_under_agent_directory() {
    let agents_dir = temp_dir("agents");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let outcome = store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "hello"))
        .expect("append agent event");

    assert_eq!(outcome.seq.get(), 0);
    assert_eq!(outcome.folded_node_id.map(|id| id.get()), Some(0));
    assert!(agents_dir.join("agent-1").join("events.cbor").exists());

    let reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree");
    assert_eq!(tree.agent_id(), "agent-1");
    assert_eq!(tree.current_branch().len(), 1);
    assert!(matches!(
        tree.current_branch()[0],
        AgentEntry::UserInput { .. }
    ));

    let events = reopened.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, agent_prompt("agent-1", "hello"));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_non_sequential_persisted_sequence_on_load() {
    let agents_dir = temp_dir("agents-bad-seq");
    let events_path = agents_dir.join("agent-1").join("events.cbor");

    // Persisted sequence is deliberately redundant with file order. Loading must
    // reject a mismatch so a reordered or spliced event stream is caught before
    // it is folded into the agent tree.
    append_raw_cbor(
        &events_path,
        &PersistedAgentEvent {
            seq: PersistedAgentEventSeq::new(1),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::InheritHead,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = AgentStore::open(&agents_dir).expect_err("bad sequence must fail load");
    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_replays_explicit_root_parent_after_reopen() {
    let agents_dir = temp_dir("agents-explicit-root-parent");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "second"))
        .expect("append second prompt");
    store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Root,
            agent_prompt("agent-1", "fresh branch"),
            tau_proto::UnixMicros::now(),
        )
        .expect("append fresh branch prompt");

    let reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree");
    let fresh_branch = tree.nodes().last().expect("fresh branch node");

    assert_eq!(fresh_branch.parent_id, None);
    assert_eq!(tree.head(), Some(fresh_branch.id));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_unknown_explicit_parent_before_persisting() {
    let agents_dir = temp_dir("agents-unknown-explicit-parent");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");

    let error = store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Under(NodeId::new(999)),
            agent_prompt("agent-1", "dangling parent"),
            tau_proto::UnixMicros::now(),
        )
        .expect_err("agent store must reject unknown explicit parents");
    match error {
        AgentStoreError::InvalidEvent { source } => {
            assert!(source.to_string().contains("unknown node_id: 999"));
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 1);
    let tree = store.agent("agent-1").expect("agent tree");
    assert_eq!(tree.nodes().len(), 1);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_restores_head_move_before_next_append() {
    let agents_dir = temp_dir("agents-head-move");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "second"))
        .expect("append second prompt");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentHeadMoved(AgentHeadMoved {
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                head: AgentHead::Node(NodeId::new(0)),
            }),
        )
        .expect("persist head move");
    drop(store);

    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree after reopen");
    assert_eq!(tree.head(), Some(NodeId::new(0)));

    reopened
        .append_agent_event(
            "agent-1",
            None,
            agent_prompt("agent-1", "branched after resume"),
        )
        .expect("append resumed branch prompt");

    let tree = reopened.agent("agent-1").expect("agent tree after append");
    let branched = tree.nodes().last().expect("branched node");
    assert_eq!(branched.parent_id, Some(NodeId::new(0)));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_restores_root_head_move_before_next_append() {
    // Rewinding to before the first prompt is represented by a durable
    // `AgentHead::Root` head move. Replaying the agent log must preserve that
    // root cursor so the next user prompt starts a new root branch after
    // restart instead of inheriting the previous leaf.
    let agents_dir = temp_dir("agents-root-head-move");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentHeadMoved(AgentHeadMoved {
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                head: AgentHead::Root,
            }),
        )
        .expect("persist root head move");
    drop(store);

    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree after reopen");
    assert_eq!(tree.head(), None);

    reopened
        .append_agent_event(
            "agent-1",
            None,
            agent_prompt("agent-1", "branched from root after resume"),
        )
        .expect("append resumed root branch prompt");

    let tree = reopened.agent("agent-1").expect("agent tree after append");
    let branched = tree.nodes().last().expect("branched node");
    assert_eq!(branched.parent_id, None);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn session_store_persists_only_membership_facts() {
    let sessions_dir = temp_dir("sessions");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let loaded = Event::SessionAgentLoaded(SessionAgentLoaded {
        session_id: SessionId::from("session-1"),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
    });
    let outcome = store
        .append_session_event("session-1", None, loaded.clone())
        .expect("append loaded");

    assert_eq!(outcome.seq.get(), 0);
    assert_eq!(outcome.folded_node_id, None);
    assert!(sessions_dir.join("session-1").join("events.cbor").exists());
    assert!(
        store
            .session("session-1")
            .expect("session membership")
            .contains_agent(&AgentId::parse("agent-1").expect("agent id"))
    );

    store
        .append_session_event(
            "session-1",
            None,
            Event::SessionAgentUnloaded(SessionAgentUnloaded {
                session_id: SessionId::from("session-1"),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
            }),
        )
        .expect("append unloaded");

    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    let membership = reopened.session("session-1").expect("session membership");
    assert_eq!(membership.session_id(), "session-1");
    assert!(!membership.contains_agent(&AgentId::parse("agent-1").expect("agent id")));
    let events = reopened.session_events("session-1").expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, loaded);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_rejects_non_sequential_persisted_sequence_on_load() {
    let sessions_dir = temp_dir("sessions-bad-seq");
    let events_path = sessions_dir.join("session-1").join("events.cbor");

    // Persisted sequence is deliberately redundant with file order. Loading must
    // reject a mismatch so a reordered or spliced membership stream is caught
    // before it is folded into the session view.
    append_raw_cbor(
        &events_path,
        &PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(1),
            source: None,
            event: Event::SessionAgentLoaded(SessionAgentLoaded {
                session_id: SessionId::from("session-1"),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
            }),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = SessionStore::open(&sessions_dir).expect_err("bad sequence must fail load");
    assert!(matches!(error, SessionStoreError::InvalidSequence { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn agent_store_rejects_non_agent_transcript_events() {
    let agents_dir = temp_dir("agent-rejects-non-transcript");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let session_event = Event::SessionAgentLoaded(SessionAgentLoaded {
        session_id: SessionId::from("session-1"),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
    });
    let error = store
        .append_agent_event("agent-1", None, session_event)
        .expect_err("agent store must reject session membership events");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));

    let mismatched = agent_prompt("agent-2", "not this agent");
    let error = store
        .append_agent_event("agent-1", None, mismatched)
        .expect_err("agent store must reject mismatched agent events");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert!(!agents_dir.join("agent-1").join("events.cbor").exists());

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn session_store_rejects_transcript_events() {
    let sessions_dir = temp_dir("session-rejects-transcript");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let error = store
        .append_session_event("session-1", None, agent_prompt("agent-1", "not membership"))
        .expect_err("session store must reject transcript events");

    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    assert!(!sessions_dir.join("session-1").join("events.cbor").exists());

    let _ = std::fs::remove_dir_all(sessions_dir);
}
