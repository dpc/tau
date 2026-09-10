use std::io as path_std_io;
use std::time::{Duration, SystemTime};

use tau_core::{AgentEventParent, AgentStore, SessionStore};
use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionId, UnixMicros};
use tempfile::TempDir;

use crate::artifact_store::ArtifactStore;

/// A persistent harness with both journal domains ephemeral still cleans shared
/// artifacts while leaving durable session and agent trees untouched.
#[test]
fn artifact_cleanup_runs_with_ephemeral_session_and_agent_persistence() {
    let temp = TempDir::new().expect("temp state");
    seed_agent_and_session(&temp);
    let mut store = ArtifactStore::new(temp.path());
    let tau_proto::ArtifactValue::Upload { upload } = store
        .execute(
            "tool/session",
            "connection",
            tau_proto::ArtifactOp::Begin {
                size: tau_proto::ArtifactSize::new(0).expect("size"),
            },
            1,
        )
        .expect("begin empty original")
    else {
        panic!("upload response")
    };
    let tau_proto::ArtifactValue::Descriptor(descriptor) = store
        .execute(
            "tool/session",
            "connection",
            tau_proto::ArtifactOp::Finalize { upload },
            1,
        )
        .expect("publish empty original")
    else {
        panic!("descriptor response")
    };
    super::run_retention_cleanup(
        super::RetentionCleanup {
            memory_only: false,
            artifact_retention: Some(Duration::from_secs(1)),
            state_dir: temp.path().to_path_buf(),
            sessions_dir: temp.path().join("sessions"),
            session_persistence: tau_core::SessionPersistenceMode::Ephemeral,
            agent_persistence: tau_core::AgentPersistenceMode::Ephemeral,
            current_session: "current".parse().expect("session"),
            session_retention: Some(Duration::from_secs(1)),
            agent_retention: Some(Duration::from_secs(1)),
            diagnostic_retention: None,
        },
        SystemTime::UNIX_EPOCH + Duration::from_secs(100),
    );
    assert_eq!(
        store.execute(
            "tool/session",
            "connection",
            tau_proto::ArtifactOp::Stat {
                key: descriptor.key
            },
            100
        ),
        Err(tau_proto::ArtifactError::Unavailable)
    );
    assert!(temp.path().join("sessions/owner-session").exists());
    assert!(temp.path().join("agents/owned-agent").exists());
}

fn seed_agent_and_session(temp: &TempDir) -> AgentId {
    let agent_id = AgentId::parse("owned-agent").expect("agent id");
    let mut agents = AgentStore::open_fixture(temp.path().join("agents")).expect("agent store");
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
    let mut sessions =
        SessionStore::open_fixture(temp.path().join("sessions")).expect("session store");
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
            memory_only: false,
            artifact_retention: None,
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
            memory_only: false,
            artifact_retention: None,
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

/// A session detached without a durable source-parent boundary cannot disappear
/// from agent reference authority until a later startup finalizes that detach.
#[test]
fn uncertain_session_detach_suppresses_agent_deletion_until_restart_finalization() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = seed_agent_and_session(&temp);
    let cleanup = super::RetentionCleanup {
        memory_only: false,
        artifact_retention: None,
        state_dir: temp.path().to_path_buf(),
        sessions_dir: temp.path().join("sessions"),
        session_persistence: tau_core::SessionPersistenceMode::Durable,
        agent_persistence: tau_core::AgentPersistenceMode::Durable,
        current_session: SessionId::parse("current").expect("session id"),
        session_retention: Some(Duration::from_secs(1)),
        agent_retention: Some(Duration::from_secs(1)),
        diagnostic_retention: None,
    };
    let now = SystemTime::now() + Duration::from_secs(60);
    let mut sync_calls = 0;

    super::run_retention_cleanup_with_session_cleanup(
        cleanup.clone(),
        now,
        |sessions_dir, retention, protected, now| {
            crate::session_cleanup::cleanup_old_sessions_with_hooks(
                sessions_dir,
                retention,
                protected,
                now.duration_since(SystemTime::UNIX_EPOCH)
                    .expect("post-epoch clock")
                    .as_secs(),
                |path| std::fs::remove_dir_all(path),
                |_| {},
                |path| {
                    sync_calls += 1;
                    if sync_calls == 4 {
                        Err(path_std_io::Error::other(
                            "injected post-rename source-parent sync failure",
                        ))
                    } else {
                        crate::retention_fs::sync_directory(path)
                    }
                },
            )
        },
    );

    assert!(!temp.path().join("sessions/owner-session").exists());
    assert_eq!(
        std::fs::read_dir(temp.path().join(".sessions.cleanup"))
            .expect("detached session staging")
            .count(),
        1
    );
    assert!(temp.path().join("agents/owned-agent").exists());
    assert!(!tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());

    super::run_retention_cleanup(cleanup, now);

    assert_eq!(
        std::fs::read_dir(temp.path().join(".sessions.cleanup"))
            .expect("finalized session staging")
            .count(),
        0
    );
    assert!(!temp.path().join("agents/owned-agent").exists());
    assert!(tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}
