use std::collections::BTreeSet;

use tau_swarm_api::{
    AgentActivity, AgentNavigationMode, TaskId, Timestamp, UpdateId, UpdatePublication,
};

use super::*;

/// Ensures snapshots and incremental batches represent the same
/// replacement.
#[test]
fn publishes_agent_replacement() {
    let mut projection = SessionProjection::new(8);
    let agent = Agent {
        id: AgentId::new("a"),
        name: "Agent".into(),
        activity: AgentActivity::Waiting,
        navigation_mode: AgentNavigationMode::ActiveAuto,
        watches: BTreeSet::new(),
    };
    projection
        .upsert_agent(agent.clone())
        .expect("publication fits");
    let snapshot = projection.snapshot();
    assert_eq!(snapshot.revision, PublicationRevision(1));
    assert_eq!(
        snapshot.snapshot.agents.as_slice(),
        std::slice::from_ref(&agent)
    );
    let batch = projection
        .changes_after(PublicationRevision(0))
        .expect("retained");
    assert_eq!(batch.changes, [SessionChange::UpsertAgent(agent)]);
}

/// Ensures bounded history forces a stale reader to reconnect from a
/// snapshot.
#[test]
fn rejects_stale_revision() {
    let mut projection = SessionProjection::new(1);
    for id in ["a", "b"] {
        projection
            .upsert_agent(Agent {
                id: AgentId::new(id),
                name: id.into(),
                activity: AgentActivity::Running,
                navigation_mode: AgentNavigationMode::Active,
                watches: BTreeSet::new(),
            })
            .expect("publication fits");
    }
    assert!(projection.changes_after(PublicationRevision(0)).is_none());
}

/// Update IDs are immutable: an exact replay is harmless and a different
/// payload cannot replace the retained outbox entry.
#[test]
fn deduplicates_updates_by_immutable_payload() {
    let mut projection = SessionProjection::new(8);
    let update = UpdatePublication {
        id: UpdateId::new("update"),
        owner: AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        task_id: Some(TaskId::new("task")),
        source_timestamp: Timestamp(1),
    };
    projection.add_update(update.clone()).expect("first update");
    projection.add_update(update.clone()).expect("exact replay");
    assert!(
        projection
            .pending_updates_through(PublicationRevision(0))
            .is_empty()
    );
    let pending = projection.pending_updates_through(PublicationRevision(1));
    assert_eq!(pending.as_slice(), std::slice::from_ref(&update));
    let mut changed = update;
    changed.description = "different".into();
    assert!(projection.add_update(changed).is_err());
    assert_eq!(projection.update_usage().0, 1);
    projection.acknowledge_update(&UpdateId::new("update"));
    assert_eq!(projection.update_usage(), (0, 0));
}

/// Change-history byte limits count logical UTF-8 fields rather than bincode
/// framing bytes.
#[test]
fn evicts_change_history_by_logical_string_bytes() {
    let mut projection = SessionProjection::new(8).with_byte_limits(1, usize::MAX);
    projection
        .upsert_agent(Agent {
            id: AgentId::new("a"),
            name: "b".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
        })
        .expect("publication");
    assert!(projection.changes_after(PublicationRevision(0)).is_none());
}
