use std::time::{Duration, SystemTime};

use tau_core::{AgentEventParent, AgentStore, SessionStore};
use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionId, UnixMicros};
use tempfile::TempDir;

fn seed_agent_and_session(temp: &TempDir) -> AgentId {
    let agent_id = AgentId::parse("owned-agent").expect("agent id");
    let mut agents = AgentStore::open_lazy(temp.path().join("agents")).expect("agent store");
    agents
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(1),
        )
        .expect("append agent");
    drop(agents);
    let session_id = SessionId::parse("owner-session").expect("session id");
    let mut sessions = SessionStore::open(temp.path().join("sessions")).expect("session store");
    sessions
        .append_session_event(
            session_id.as_str(),
            None,
            Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("init")
                    .expect("initialization id"),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("append membership");
    drop(sessions);
    std::fs::write(
        temp.path().join("sessions/owner-session/meta.json"),
        serde_json::to_vec(&tau_core::SessionMeta {
            created_at: 1,
            last_touched: 1,
        })
        .expect("meta"),
    )
    .expect("age session");
    agent_id
}

/// Session cleanup removes the last ownership edge before the same pass decides
/// that the old agent is an orphan.
#[test]
fn session_deletion_precedes_agent_reference_authority() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = seed_agent_and_session(&temp);
    super::run_retention_cleanup(
        super::RetentionCleanup {
            state_dir: temp.path().to_path_buf(),
            sessions_dir: temp.path().join("sessions"),
            session_persistence: tau_core::SessionPersistenceMode::Durable,
            agent_persistence: tau_core::AgentPersistenceMode::Durable,
            current_session: SessionId::parse("current").expect("session id"),
            session_retention: Some(Duration::from_secs(1)),
            agent_retention: Some(Duration::from_secs(1)),
            diagnostic_retention: None,
        },
        SystemTime::now() + Duration::from_secs(60),
    );

    assert!(!temp.path().join("sessions/owner-session").exists());
    assert!(!temp.path().join("agents/owned-agent").exists());
    assert!(tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}

/// A disabled agent policy makes no new eligibility decision.
#[test]
fn disabled_agent_policy_preserves_live_agent_tree() {
    let temp = TempDir::new().expect("temp state");
    seed_agent_and_session(&temp);
    let detached_agent = temp.path().join(".agents.cleanup/stale");
    let detached_session = temp.path().join(".sessions.cleanup/stale");
    std::fs::create_dir_all(&detached_agent).expect("detached agent staging");
    std::fs::create_dir_all(&detached_session).expect("detached session staging");
    super::run_retention_cleanup(
        super::RetentionCleanup {
            state_dir: temp.path().to_path_buf(),
            sessions_dir: temp.path().join("sessions"),
            session_persistence: tau_core::SessionPersistenceMode::Durable,
            agent_persistence: tau_core::AgentPersistenceMode::Durable,
            current_session: SessionId::parse("current").expect("session id"),
            session_retention: None,
            agent_retention: None,
            diagnostic_retention: None,
        },
        SystemTime::now() + Duration::from_secs(60),
    );

    assert!(temp.path().join("agents/owned-agent").exists());
    assert!(!detached_agent.exists());
    assert!(!detached_session.exists());
}
