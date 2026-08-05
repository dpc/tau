//! Focused tests for the enclosing runtime component.

use tau_proto::{AgentCreator, AgentId, EstimatedApiCost, SessionId};

use super::AgentCostLedger;
use crate::agent_creator_topology::{AgentCreatorTopology, RecordCreatorOutcome};

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("test agent id is valid")
}

fn session_id() -> SessionId {
    SessionId::parse("session").expect("test session id is valid")
}

fn record_edge(topology: &mut AgentCreatorTopology, child: &str, creator: &str) {
    let session_id = session_id();
    assert_eq!(
        topology.record(
            agent_id(child),
            Some(&AgentCreator::Agent {
                session_id: session_id.clone(),
                agent_id: agent_id(creator),
            }),
            &session_id,
        ),
        RecordCreatorOutcome::Recorded
    );
}

/// Charges a response to its agent and every authenticated creator ancestor
/// while preserving independently saturating self and inclusive totals.
#[test]
fn increments_propagate_to_creator_ancestors() {
    let mut topology = AgentCreatorTopology::default();
    record_edge(&mut topology, "child", "parent");
    record_edge(&mut topology, "grandchild", "child");
    let mut ledger = AgentCostLedger::default();
    assert_eq!(
        ledger.add_increment(
            &agent_id("grandchild"),
            EstimatedApiCost::from_picodollars(7),
            &topology,
        ),
        vec![
            agent_id("grandchild"),
            agent_id("child"),
            agent_id("parent")
        ]
    );
    assert_eq!(
        ledger.self_cost(&agent_id("grandchild")).as_picodollars(),
        7
    );
    assert_eq!(ledger.self_cost(&agent_id("child")).as_picodollars(), 0);
    assert_eq!(
        ledger
            .creator_subtree_cost(&agent_id("parent"))
            .as_picodollars(),
        7
    );
}

/// Charges that predate a newly discovered valid edge flow to its creator
/// exactly once, which keeps resume and delayed creation folding coherent.
#[test]
fn attaching_existing_subtree_propagates_once() {
    let mut topology = AgentCreatorTopology::default();
    let mut ledger = AgentCostLedger::default();
    ledger.add_increment(
        &agent_id("child"),
        EstimatedApiCost::from_picodollars(5),
        &topology,
    );
    record_edge(&mut topology, "child", "parent");
    ledger.attach_existing_subtree(&agent_id("child"), &topology);
    assert_eq!(
        ledger
            .creator_subtree_cost(&agent_id("parent"))
            .as_picodollars(),
        5
    );
    ledger.add_increment(
        &agent_id("child"),
        EstimatedApiCost::from_picodollars(u64::MAX),
        &topology,
    );
    assert_eq!(
        ledger
            .creator_subtree_cost(&agent_id("parent"))
            .as_picodollars(),
        u64::MAX
    );
}
