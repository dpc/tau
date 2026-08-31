use std::collections::BTreeSet;

use tau_swarm_api::{
    AgentActivity, AgentNavigationMode, AgentWorkStatus, TaskDescription, TaskId, TaskInfo,
    TaskTitle, Timestamp, UpdateId, UpdatePublication,
};

use super::*;

/// Builds valid canonical metadata for projection tests.
fn task_info(id: impl Into<String>, title: impl Into<String>) -> TaskInfo {
    TaskInfo {
        task_id: TaskId::new(id.into()),
        title: TaskTitle::new(title.into()).expect("valid title"),
        description: None,
    }
}

/// Builds a projection with only its retained-change entry bound varied.
fn projection_with_history(history_entries: usize) -> SessionProjection {
    SessionProjection::new(ProjectionLimits {
        history_entries,
        ..ProjectionLimits::unconfigured()
    })
}

/// Ensures snapshots and incremental batches represent the same
/// replacement.
#[test]
fn publishes_agent_replacement() {
    let mut projection = projection_with_history(8);
    let agent = Agent {
        id: AgentId::new("a"),
        name: "Agent".into(),
        activity: AgentActivity::Waiting,
        navigation_mode: AgentNavigationMode::ActiveAuto,
        watches: BTreeSet::new(),
        work_status: AgentWorkStatus::Unreported,
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
    let mut projection = projection_with_history(1);
    for id in ["a", "b"] {
        projection
            .upsert_agent(Agent {
                id: AgentId::new(id),
                name: id.into(),
                activity: AgentActivity::Running,
                navigation_mode: AgentNavigationMode::Active,
                watches: BTreeSet::new(),
                work_status: AgentWorkStatus::Unreported,
            })
            .expect("publication fits");
    }
    assert!(projection.changes_after(PublicationRevision(0)).is_none());
}

/// Update IDs are immutable: an exact replay is harmless and a different
/// payload cannot replace the retained outbox entry.
#[test]
fn deduplicates_updates_by_immutable_payload() {
    let mut projection = projection_with_history(8);
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
    assert_eq!(projection.update_usage().entries, 1);
    projection.acknowledge_update(&UpdateId::new("update"));
    assert_eq!(
        projection.update_usage(),
        UpdateUsage {
            entries: 0,
            logical_bytes: 0,
        }
    );
}

/// Change-history byte limits count logical UTF-8 fields rather than bincode
/// framing bytes.
#[test]
fn keeps_logical_history_and_encoded_publication_limits_distinct() {
    let mut projection = SessionProjection::new(ProjectionLimits {
        history_entries: 8,
        history_bytes: 1,
        publication_bytes: usize::MAX,
        task_info_entries: tau_swarm_api::MAX_TASK_INFO_ENTRIES,
    });
    projection
        .upsert_agent(Agent {
            id: AgentId::new("a"),
            name: "b".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("publication");
    assert!(projection.changes_after(PublicationRevision(0)).is_none());
}

/// Task metadata replacement participates in the same revision order as
/// immutable updates and appears identically in snapshot and live views.
#[test]
fn task_info_replacement_shares_projection_order() {
    let mut projection = projection_with_history(8);
    let first = task_info("task", "First");
    projection
        .upsert_task_info(first.clone())
        .expect("first task metadata");
    projection
        .add_update(UpdatePublication {
            id: UpdateId::new("update"),
            owner: AgentId::new("agent"),
            title: "update".into(),
            description: "description".into(),
            task_id: Some(TaskId::new("task")),
            source_timestamp: Timestamp(1),
        })
        .expect("ordered update");
    let replacement = TaskInfo {
        task_id: TaskId::new("task"),
        title: TaskTitle::new("Second").expect("title"),
        description: Some(TaskDescription::new("details").expect("description")),
    };
    projection
        .upsert_task_info(replacement.clone())
        .expect("replacement");

    assert_eq!(
        projection.snapshot().snapshot.task_info.as_slice(),
        std::slice::from_ref(&replacement)
    );
    assert_eq!(
        projection
            .changes_after(PublicationRevision(0))
            .expect("retained")
            .changes,
        [
            SessionChange::UpsertTaskInfo(first),
            SessionChange::AddUpdate(
                projection
                    .pending_updates_through(PublicationRevision(3))
                    .into_iter()
                    .next()
                    .expect("pending update")
            ),
            SessionChange::UpsertTaskInfo(replacement)
        ]
    );
}

/// An equal task-info upsert is harmless and need not create a second view
/// mutation, while a changed replacement advances the revision.
#[test]
fn equal_task_info_upsert_does_not_advance_revision() {
    let mut projection = projection_with_history(8);
    let info = task_info("task", "Title");
    projection
        .upsert_task_info(info.clone())
        .expect("first metadata");
    let before_snapshot = projection.snapshot();
    let before_changes = projection
        .changes_after(PublicationRevision(0))
        .expect("retained first metadata");

    projection
        .upsert_task_info(info)
        .expect("equal replacement");
    assert_eq!(projection.snapshot(), before_snapshot);
    assert_eq!(
        projection
            .changes_after(PublicationRevision(0))
            .expect("retained unchanged metadata"),
        before_changes
    );

    let replacement = task_info("task", "Replacement");
    projection
        .upsert_task_info(replacement.clone())
        .expect("changed replacement");
    assert_eq!(projection.snapshot().revision, PublicationRevision(2));
    assert_eq!(
        projection
            .changes_after(PublicationRevision(1))
            .expect("retained replacement")
            .changes,
        [SessionChange::UpsertTaskInfo(replacement)]
    );
}

/// Replacement remains allowed at the configured entry ceiling, but a distinct
/// task fails without changing the complete projection transaction.
#[test]
fn task_info_entry_limit_is_transactional() {
    let mut projection = SessionProjection::new(ProjectionLimits {
        history_entries: 8,
        task_info_entries: 1,
        ..ProjectionLimits::unconfigured()
    });
    projection
        .upsert_task_info(task_info("task", "First"))
        .expect("first metadata");
    projection
        .upsert_task_info(task_info("task", "Replacement"))
        .expect("replacement at ceiling");
    let before = projection.snapshot();
    assert_eq!(
        projection.upsert_task_info(task_info("second", "Second")),
        Err("task info entry limit is full")
    );
    assert_eq!(projection.snapshot(), before);
    assert_eq!(projection.task_info.len(), 1);
}

/// Aggregate canonical-content overflow and publication-revision exhaustion
/// both fail before changing map, history, or revision.
#[test]
fn task_info_capacity_and_revision_fail_closed() {
    let mut projection = projection_with_history(8);
    let description = TaskDescription::new("x".repeat(16_384)).expect("maximum description");
    for index in 0..511 {
        let info = TaskInfo {
            task_id: TaskId::new(format!("task-{index}")),
            title: TaskTitle::new("t").expect("title"),
            description: Some(description.clone()),
        };
        projection.task_info.insert(info.task_id.clone(), info);
    }
    assert!(
        tau_swarm_api::task_info_bytes(projection.task_info.values()).expect("valid existing map")
            <= tau_swarm_api::MAX_TASK_INFO_BYTES
    );
    let before_snapshot = projection.snapshot();
    let before_changes = projection.changes.clone();
    assert_eq!(
        projection.upsert_task_info(TaskInfo {
            task_id: TaskId::new("overflow"),
            title: TaskTitle::new("t").expect("title"),
            description: Some(description),
        }),
        Err("task info byte limit is full")
    );
    assert_eq!(projection.snapshot(), before_snapshot);
    assert_eq!(projection.changes, before_changes);

    let mut exhausted = projection_with_history(8);
    exhausted.revision = PublicationRevision(u64::MAX);
    let before = exhausted.snapshot();
    assert_eq!(
        exhausted.upsert_task_info(task_info("task", "Title")),
        Err("publication revision exhausted")
    );
    assert_eq!(exhausted.snapshot(), before);
}

/// Exact v0 snapshot and change encoding bounds are checked prospectively, so
/// an encoded-size rejection leaves no metadata behind.
#[test]
fn task_info_encoded_bounds_fail_closed() {
    let mut projection = SessionProjection::new(ProjectionLimits {
        history_entries: 8,
        history_bytes: usize::MAX,
        publication_bytes: 1,
        task_info_entries: tau_swarm_api::MAX_TASK_INFO_ENTRIES,
    });
    let before = projection.snapshot();
    assert!(
        projection
            .upsert_task_info(task_info("task", "Title"))
            .is_err()
    );
    assert_eq!(projection.snapshot(), before);
}

/// Revision exhaustion leaves every existing agent and blocker mutation path
/// unchanged, including both insertion and removal.
#[test]
fn agent_and_blocker_revision_exhaustion_is_transactional() {
    let agent = Agent {
        id: AgentId::new("agent"),
        name: "Agent".into(),
        activity: AgentActivity::Waiting,
        navigation_mode: AgentNavigationMode::Active,
        watches: BTreeSet::new(),
        work_status: AgentWorkStatus::Unreported,
    };
    let blocker = BlockerPublication {
        blocker_id: BlockerId::new("blocker"),
        revision: BlockerRevisionNumber(1),
        owner: AgentId::new("agent"),
        title: "Title".into(),
        description: "Description".into(),
        recommended_answer: None,
        task_id: None,
        source_timestamp: Timestamp(1),
    };

    let mut projection = projection_with_history(8);
    projection.revision = PublicationRevision(u64::MAX);
    assert_eq!(
        projection.upsert_agent(agent.clone()),
        Err("publication revision exhausted")
    );
    assert!(projection.agents.is_empty());
    assert_eq!(
        projection.add_blocker(blocker.clone()),
        Err("publication revision exhausted")
    );
    assert!(projection.blockers.is_empty());

    projection.agents.insert(agent.id.clone(), agent.clone());
    projection
        .blockers
        .insert(blocker.blocker_id.clone(), blocker.clone());
    let before = projection.snapshot();
    assert_eq!(
        projection.remove_agent(&agent.id),
        Err("publication revision exhausted")
    );
    assert_eq!(
        projection.remove_blocker(&blocker.blocker_id, Some("reason".into())),
        Err("publication revision exhausted")
    );
    assert_eq!(projection.snapshot(), before);
    assert!(projection.changes.is_empty());
}
