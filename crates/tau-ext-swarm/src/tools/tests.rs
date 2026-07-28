use std::collections::{BTreeMap, BTreeSet};

use tau_proto::SecretValue;
use tau_swarm_api::{Agent, AgentActivity, AgentNavigationMode};
use tau_swarm_client::Application;
use tau_swarm_client_api::v2::BlockerAnswerKind;
use tau_swarm_client_api::{AnswerBlockerRequest, AnswerBlockerResponse};

use super::*;
use crate::application::SwarmApplication;

fn configured_runtime() -> SwarmRuntime {
    let peer_id = iroh::SecretKey::generate().public().to_string();
    let config: crate::config::ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "host"
    }))
    .expect("config shape");
    let mut state = SwarmRuntime::new();
    state.config = Some(
        config
            .resolve(&BTreeMap::from([(
                "swarm".into(),
                SecretValue::new("secret"),
            )]))
            .expect("resolved config"),
    );
    state.replay_complete = true;
    state
        .projection
        .blocking_lock()
        .upsert_agent(Agent {
            id: tau_swarm_api::AgentId::new("agent"),
            name: "Agent".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
        })
        .expect("owner projection");
    state
}

/// The tagged action enum rejects fields belonging to another action instead
/// of deferring an ambiguous option bag to runtime validation.
#[test]
fn blocker_actions_reject_cross_action_fields() {
    assert!(
        serde_json::from_value::<BlockerArgs>(serde_json::json!({
            "action": "list",
            "reason": "not valid for list"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<BlockerArgs>(serde_json::json!({
            "action": "add",
            "title": "title",
            "description": "description",
            "blocker_id": "wrong-kind"
        }))
        .is_err()
    );
}

/// Once remote answer delivery reserves an active blocker, local cancellation
/// cannot win a second lifecycle transition.
#[test]
fn cancellation_rejects_reserved_answer() {
    let mut state = configured_runtime();
    let publication = BlockerPublication {
        blocker_id: BlockerId::new("blocker"),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        source_timestamp: Timestamp(1),
    };
    state
        .projection
        .blocking_lock()
        .add_blocker(publication)
        .expect("active blocker");
    state
        .blocker_history
        .lock()
        .expect("history")
        .push(BlockerRecord {
            blocker_id: BlockerId::new("blocker"),
            revision: BlockerRevisionNumber(1),
            owner: tau_swarm_api::AgentId::new("agent"),
            title: "title".into(),
            description: "description".into(),
            recommended_answer: None,
            task_id: None,
            state: BlockerState::Active,
            answer: None,
            answer_kind: None,
            reason: None,
            reserved_answer_bytes: 1,
        });
    assert_eq!(
        cancel_blocker(&mut state, "agent", "blocker".into(), None),
        Err("blocker answer is already pending".into())
    );
}

/// Add/cancel prospective history must preserve bytes already reserved by a
/// concurrently pending answer for the same owner.
#[test]
fn owner_history_budget_includes_pending_answer_reservations() {
    let record = BlockerRecord {
        blocker_id: BlockerId::new("blocker"),
        revision: BlockerRevisionNumber(1),
        owner: tau_swarm_api::AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        state: BlockerState::Active,
        answer: None,
        answer_kind: None,
        reason: None,
        reserved_answer_bytes: 7,
    };
    let history = vec![record.clone()];
    let mut prospective = history.clone();
    prospective[0].state = BlockerState::Cancelled;
    prospective[0].reason = Some("reason".into());
    let encoded = serde_json::to_vec(&prospective)
        .expect("history encoding")
        .len();
    assert_eq!(
        owner_history_fits(&history, "agent", &prospective, encoded + 7),
        Ok(true)
    );
    assert_eq!(
        owner_history_fits(&history, "agent", &prospective, encoded + 6),
        Ok(false)
    );
}

/// Update tool admission frees capacity only after Swarm acknowledgement and
/// rejects overflow before publication.
#[test]
fn update_tool_enforces_and_releases_outbox_capacity() {
    let mut state = configured_runtime();
    state.config.as_mut().expect("config").limits.update_entries = 1;
    let first = add_update(
        &mut state,
        "agent",
        UpdateArgs {
            title: "first".into(),
            description: "description".into(),
            task_id: None,
        },
    )
    .expect("first update");
    assert!(
        add_update(
            &mut state,
            "agent",
            UpdateArgs {
                title: "second".into(),
                description: "description".into(),
                task_id: None,
            },
        )
        .is_err()
    );
    let id = first
        .get("update_id")
        .and_then(serde_json::Value::as_str)
        .expect("update ID");
    state
        .projection
        .blocking_lock()
        .acknowledge_update(&UpdateId::new(id));
    add_update(
        &mut state,
        "agent",
        UpdateArgs {
            title: "second".into(),
            description: "description".into(),
            task_id: None,
        },
    )
    .expect("capacity after acknowledgement");
}

/// Blocker tool state crosses the real application answer boundary, updates
/// list history after canonical acceptance, deduplicates retry, and rejects a
/// stale revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_tool_answer_lifecycle_and_deduplication() {
    let mut state = tokio::task::block_in_place(configured_runtime);
    let added = tokio::task::block_in_place(|| {
        add_blocker(
            &mut state,
            "agent",
            "title".into(),
            "description".into(),
            None,
            None,
        )
    })
    .expect("add blocker");
    let blocker_id = added
        .get("blocker_id")
        .and_then(serde_json::Value::as_str)
        .expect("blocker ID")
        .to_owned();
    let (_prompt_tx, _prompt_rx) = tokio::sync::mpsc::channel(2);
    let (blocker_tx, mut blockers) = tokio::sync::mpsc::channel(2);
    let application = std::sync::Arc::new(
        SwarmApplication::new(
            tau_swarm_api::SessionIdentity::new(
                tau_swarm_api::Hostname::new("host"),
                tau_swarm_api::SessionId::new("session"),
            ),
            std::sync::Arc::clone(&state.projection),
            std::sync::Arc::clone(&state.changed),
            _prompt_tx,
            blocker_tx,
        )
        .with_blocker_history(
            std::sync::Arc::clone(&state.blocker_history),
            4 * 1024 * 1024,
        ),
    );
    let request = AnswerBlockerRequest {
        command_id: "command".into(),
        blocker_id: blocker_id.clone(),
        revision: 1,
        kind: BlockerAnswerKind::Custom,
        response: "answer".into(),
    };
    let answer = tokio::spawn({
        let application = std::sync::Arc::clone(&application);
        let request = request.clone();
        async move { application.answer_blocker(request).await }
    });
    blockers
        .recv()
        .await
        .expect("answer submission")
        .completion
        .send(Ok(()))
        .expect("canonical acceptance");
    assert_eq!(
        answer.await.expect("answer task"),
        Ok(AnswerBlockerResponse::Accepted)
    );
    let listed = tokio::task::block_in_place(|| list_blockers(&state, "agent"))
        .expect("list")
        .as_array()
        .expect("history array")
        .clone();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].get("state").and_then(serde_json::Value::as_str),
        Some("answered")
    );
    assert_eq!(
        application.answer_blocker(request.clone()).await,
        Ok(AnswerBlockerResponse::Accepted)
    );
    assert!(blockers.try_recv().is_err());
    let mut stale = request;
    stale.command_id = "stale".into();
    stale.revision = 2;
    assert!(matches!(
        application.answer_blocker(stale).await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
    let second = tokio::task::block_in_place(|| {
        add_blocker(
            &mut state,
            "agent",
            "second".into(),
            "description".into(),
            None,
            None,
        )
    })
    .expect("second blocker");
    tokio::task::block_in_place(|| {
        state
            .projection
            .blocking_lock()
            .remove_agent(&tau_swarm_api::AgentId::new("agent"))
    })
    .expect("remove owner");
    assert!(matches!(
        application
            .answer_blocker(AnswerBlockerRequest {
                command_id: "missing-owner".into(),
                blocker_id: second
                    .get("blocker_id")
                    .and_then(serde_json::Value::as_str)
                    .expect("blocker ID")
                    .into(),
                revision: 1,
                kind: BlockerAnswerKind::Custom,
                response: "answer".into(),
            })
            .await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
}
