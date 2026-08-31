//! Tests for cost accounting behavior.

use super::*;

/// Cold restore seeds only loaded durable creation edges, then attaches an
/// already-charged pending subtree exactly once when its parent loads later.
#[test]
fn sparse_cold_restore_attaches_pending_creator_subtree_once() {
    fn started(agent_id: tau_proto::AgentId, creator: Option<tau_proto::AgentCreator>) -> Event {
        Event::AgentStarted(tau_proto::AgentStarted {
            agent_id: agent_id.clone(),
            creator,
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        })
    }

    fn load_durable_agent(harness: &mut Harness, agent_id: &tau_proto::AgentId) {
        let mut runtime = Agent::new(
            agent_id.clone(),
            harness.mint_agent_runtime_incarnation(),
            harness.session_runtime.current_session_id.clone(),
            tau_proto::PromptOriginator::User,
            None,
            None,
        );
        runtime.identity.agent_id = Some(agent_id.clone());
        harness
            .agent_runtime
            .agent_registry
            .agents
            .insert(agent_id.clone(), runtime);
        harness.ensure_loaded_agent_for_agent(agent_id, agent_id);
    }

    let td = tempfile::tempdir().expect("tempdir");
    let state_dir = td.path().join("state");
    let a = tau_proto::AgentId::parse("a").expect("test agent id");
    let b = tau_proto::AgentId::parse("b").expect("test agent id");
    let c = tau_proto::AgentId::parse("c").expect("test agent id");
    let session_id = test_session_id("s1");
    let mut harness = echo_harness(&state_dir).expect("harness");
    for (agent_id, creator) in [
        (a.clone(), Some(tau_proto::AgentCreator::User)),
        (
            b.clone(),
            Some(tau_proto::AgentCreator::Agent {
                session_id: session_id.clone(),
                agent_id: a.clone(),
            }),
        ),
        (
            c.clone(),
            Some(tau_proto::AgentCreator::Agent {
                session_id,
                agent_id: b.clone(),
            }),
        ),
    ] {
        harness
            .append_direct_agent_semantic_event(
                agent_id.as_str(),
                tau_core::AgentEventParent::InheritHead,
                started(agent_id.clone(), creator),
            )
            .expect("persist durable creation");
    }
    drop(harness);

    let mut restored =
        echo_harness_with_start_reason("s1", &state_dir, tau_proto::SessionStartReason::Resume)
            .expect("cold restore");
    load_durable_agent(&mut restored, &a);
    load_durable_agent(&mut restored, &c);
    restored
        .agent_runtime
        .agent_registry
        .cost_ledger
        .add_increment(
            &c,
            tau_proto::EstimatedApiCost::from_picodollars(4),
            &restored.agent_runtime.agent_registry.creator_topology,
        );
    for agent_id in [&c, &b] {
        assert_eq!(
            restored
                .agent_runtime
                .agent_registry
                .cost_ledger
                .creator_subtree_cost(agent_id)
                .as_picodollars(),
            4
        );
    }
    assert_eq!(
        restored
            .agent_runtime
            .agent_registry
            .cost_ledger
            .creator_subtree_cost(&a)
            .as_picodollars(),
        0
    );

    load_durable_agent(&mut restored, &b);
    for agent_id in [&a, &b, &c] {
        assert_eq!(
            restored
                .agent_runtime
                .agent_registry
                .cost_ledger
                .creator_subtree_cost(agent_id)
                .as_picodollars(),
            4,
            "the first deferred parent load must attach {agent_id}'s subtree cost"
        );
    }
    load_durable_agent(&mut restored, &b);
    load_durable_agent(&mut restored, &c);
    for agent_id in [&a, &b, &c] {
        assert_eq!(
            restored
                .agent_runtime
                .agent_registry
                .cost_ledger
                .creator_subtree_cost(agent_id)
                .as_picodollars(),
            4,
            "repeated durable creation replay must not duplicate {agent_id}'s subtree cost"
        );
    }
}

/// Session rollover discards runtime-only creator topology and cost totals
/// rather than carrying descendant accounting into the next session.
#[test]
fn session_rollover_resets_creator_subtree_cost_accounting() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness = echo_harness(td.path()).expect("harness");
    let parent = tau_proto::AgentId::parse("parent").expect("test parent id");
    let child = tau_proto::AgentId::parse("child").expect("test child id");
    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .creator_topology
            .record(
                child.clone(),
                Some(&tau_proto::AgentCreator::Agent {
                    session_id: harness.session_runtime.current_session_id.clone(),
                    agent_id: parent.clone(),
                }),
                &harness.session_runtime.current_session_id,
            ),
        RecordCreatorOutcome::Recorded
    );
    harness
        .agent_runtime
        .agent_registry
        .cost_ledger
        .add_increment(
            &child,
            tau_proto::EstimatedApiCost::from_picodollars(9),
            &harness.agent_runtime.agent_registry.creator_topology,
        );
    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .cost_ledger
            .creator_subtree_cost(&parent)
            .as_picodollars(),
        9
    );

    harness
        .switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .cost_ledger
            .creator_subtree_cost(&parent)
            .as_picodollars(),
        0
    );
    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .cost_ledger
            .add_increment(
                &child,
                tau_proto::EstimatedApiCost::from_picodollars(1),
                &harness.agent_runtime.agent_registry.creator_topology,
            ),
        vec![child]
    );
}

/// A child response refreshes complete self/subtree snapshots for the child and
/// every still-loaded authenticated creator ancestor without changing self
/// cost.
#[test]
fn descendant_cost_increment_publishes_loaded_creator_snapshots() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness = echo_harness(td.path()).expect("harness");
    let parent = tau_proto::AgentId::parse("parent").expect("test parent id");
    let child = tau_proto::AgentId::parse("child").expect("test child id");
    for agent_id in [&parent, &child] {
        let mut agent = Agent::new(
            agent_id.clone(),
            harness.mint_agent_runtime_incarnation(),
            harness.session_runtime.current_session_id.clone(),
            tau_proto::PromptOriginator::User,
            None,
            None,
        );
        agent.identity.agent_id = Some(agent_id.clone());
        harness
            .agent_runtime
            .agent_registry
            .agents
            .insert(agent_id.clone(), agent);
        harness
            .agent_runtime
            .agent_registry
            .navigation_modes
            .insert(agent_id.clone(), tau_proto::AgentNavigationMode::Active);
    }
    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .creator_topology
            .record(
                child.clone(),
                Some(&tau_proto::AgentCreator::Agent {
                    session_id: harness.session_runtime.current_session_id.clone(),
                    agent_id: parent.clone(),
                }),
                &harness.session_runtime.current_session_id,
            ),
        RecordCreatorOutcome::Recorded
    );

    harness.add_estimated_cost_increment(
        &child,
        tau_proto::EstimatedApiCost::from_picodollars(9),
        None,
    );

    let snapshots = event_log_events(&harness)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStatsUpdated(stats) => Some(stats),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].agent_id, child);
    assert_eq!(snapshots[0].estimated_api_cost.as_picodollars(), 9);
    assert_eq!(
        snapshots[0]
            .creator_subtree_estimated_api_cost
            .as_picodollars(),
        9
    );
    assert_eq!(snapshots[1].agent_id, parent);
    assert_eq!(snapshots[1].estimated_api_cost.as_picodollars(), 0);
    assert_eq!(
        snapshots[1]
            .creator_subtree_estimated_api_cost
            .as_picodollars(),
        9
    );
}

/// Loading an existing durable child after runtime topology reset re-seeds its
/// authenticated creator edge, so later cost still reaches an absent parent.
#[test]
fn existing_agent_load_reseeds_creator_cost_topology() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness = echo_harness(td.path()).expect("harness");
    let parent = tau_proto::AgentId::parse("parent").expect("test parent id");
    let child = tau_proto::AgentId::parse("child").expect("test child id");
    let started = |agent_id: tau_proto::AgentId, creator| {
        Event::AgentStarted(tau_proto::AgentStarted {
            agent_id: agent_id.clone(),
            creator,
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        })
    };
    harness
        .append_direct_agent_semantic_event(
            parent.as_str(),
            tau_core::AgentEventParent::InheritHead,
            started(parent.clone(), Some(tau_proto::AgentCreator::User)),
        )
        .expect("persist parent creation");
    harness
        .append_direct_agent_semantic_event(
            child.as_str(),
            tau_core::AgentEventParent::InheritHead,
            started(
                child.clone(),
                Some(tau_proto::AgentCreator::Agent {
                    session_id: harness.session_runtime.current_session_id.clone(),
                    agent_id: parent.clone(),
                }),
            ),
        )
        .expect("persist child creation");
    harness.agent_runtime.agent_registry.creator_topology = Default::default();
    let mut child_runtime = Agent::new(
        child.clone(),
        harness.mint_agent_runtime_incarnation(),
        harness.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    child_runtime.identity.agent_id = Some(child.clone());
    harness
        .agent_runtime
        .agent_registry
        .agents
        .insert(child.clone(), child_runtime);

    harness.ensure_loaded_agent_for_agent(&child, &child);

    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .cost_ledger
            .add_increment(
                &child,
                tau_proto::EstimatedApiCost::from_picodollars(4),
                &harness.agent_runtime.agent_registry.creator_topology,
            ),
        vec![child, parent]
    );
}
