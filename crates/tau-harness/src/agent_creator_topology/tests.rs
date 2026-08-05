//! Focused tests for the enclosing runtime component.

use tau_proto::{AgentCreator, AgentId, SessionId};

use super::{AgentCreatorTopology, RecordCreatorOutcome};

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("test agent id is valid")
}

fn session_id(value: &str) -> SessionId {
    SessionId::parse(value).expect("test session id is valid")
}

fn creator(session_id: &SessionId, creator_agent_id: &str) -> AgentCreator {
    AgentCreator::Agent {
        session_id: session_id.clone(),
        agent_id: agent_id(creator_agent_id),
    }
}

/// Records only same-session authenticated agent provenance and never
/// invents relationships for user, extension, missing, or foreign creators.
#[test]
fn only_authenticated_same_session_agent_creator_forms_edge() {
    let session = session_id("session");
    let mut topology = AgentCreatorTopology::default();
    assert_eq!(
        topology.record(
            agent_id("child"),
            Some(&creator(&session, "parent")),
            &session
        ),
        RecordCreatorOutcome::Recorded
    );
    assert_eq!(
        topology.inclusive_creator_chain(&agent_id("child")),
        vec![agent_id("child"), agent_id("parent")]
    );
    assert_eq!(
        topology.record(agent_id("user"), Some(&AgentCreator::User), &session),
        RecordCreatorOutcome::NoCreatorEdge
    );
    assert_eq!(
        topology.record(agent_id("legacy"), None, &session),
        RecordCreatorOutcome::NoCreatorEdge
    );
    assert_eq!(
        topology.record(
            agent_id("foreign"),
            Some(&creator(&session_id("other"), "parent")),
            &session,
        ),
        RecordCreatorOutcome::ForeignSession
    );
}

/// Preserves the first valid edge, accepts exact repeats, and rejects
/// self/cycle attempts without changing the retained ancestry.
#[test]
fn creator_edges_are_immutable_and_acyclic() {
    let session = session_id("session");
    let mut topology = AgentCreatorTopology::default();
    assert_eq!(
        topology.record(
            agent_id("child"),
            Some(&creator(&session, "parent")),
            &session
        ),
        RecordCreatorOutcome::Recorded
    );
    assert_eq!(
        topology.record(
            agent_id("child"),
            Some(&creator(&session, "parent")),
            &session
        ),
        RecordCreatorOutcome::AlreadyRecorded
    );
    assert_eq!(
        topology.record(
            agent_id("child"),
            Some(&creator(&session, "other")),
            &session
        ),
        RecordCreatorOutcome::Conflict {
            existing_creator: agent_id("parent"),
        }
    );
    assert_eq!(
        topology.record(agent_id("self"), Some(&creator(&session, "self")), &session),
        RecordCreatorOutcome::RejectedSelf
    );
    assert_eq!(
        topology.record(
            agent_id("parent"),
            Some(&creator(&session, "child")),
            &session
        ),
        RecordCreatorOutcome::RejectedCycle
    );
    assert_eq!(
        topology.inclusive_creator_chain(&agent_id("child")),
        vec![agent_id("child"), agent_id("parent")]
    );
}
