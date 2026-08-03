//! Lossy-hint worker for bounded Rostra following notification reports.
//!
//! Rostra broadcasts only wake this worker. The durable materialization feed
//! selects reports, while `notification_state` owns policy and checkpoints.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rostra_client::{Client, SocialPostMaterialization};
use tau_proto::{
    AgentId, CborValue, Event, MessageAgentTarget, MessageConversation, MessageDelivered,
    MessageExtensionData, MessageFactId, MessageParty, RawMessagePublisherId,
};
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use tokio::time::Instant as TokioInstant;

use crate::notification_page::ScannedPage;
use crate::notification_state::{MATERIALIZATION_PAGE, Pending, SCHEMA, State};

/// Emits a count-only wake from selected posts and the full source-page cursor.
fn report(
    publisher: RawMessagePublisherId,
    agent_id: AgentId,
    self_id: String,
    attempt: u64,
    pending: &Pending,
) -> Result<MessageDelivered<RawMessagePublisherId>, &'static str> {
    if pending.count == 0 {
        return Err("notification report requires selected posts");
    }
    let mut delivered = MessageDelivered::new(
        publisher,
        MessageAgentTarget::new(agent_id.as_ref()),
        MessageFactId::new(format!("rostra-batch-v1:{attempt}")),
        MessageParty {
            stable_id: "rostra-following".to_owned(),
            display_name: Some("Rostra following timeline".to_owned()),
            sender_auth: None,
        },
        Some(MessageConversation {
            stable_id: self_id,
            display_name: Some("Rostra following".to_owned()),
            alias: None,
        }),
        report_body(pending.count),
    );
    delivered.extension_data = MessageExtensionData::new(CborValue::Map(vec![
        (
            CborValue::Text("schema".to_owned()),
            CborValue::Text(SCHEMA.to_owned()),
        ),
        (
            CborValue::Text("scanned_through".to_owned()),
            encode_value(&pending.end)?,
        ),
    ]))
    .map_err(|_| "notification extension metadata exceeds protocol bound")?;
    Ok(delivered)
}

/// Renders the exact count-only notice carried by one report.
fn report_body(count: usize) -> String {
    match count {
        1 => "Rostra received 1 new followed post.".to_owned(),
        _ => format!("Rostra received {count} new followed posts."),
    }
}

/// Starts the lossy-hint worker; the feed, not broadcasts, supplies records.
pub(crate) fn spawn(
    runtime: &tokio::runtime::Runtime,
    client: Arc<Client>,
    handle: tau_client::ClientHandle,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
) -> AbortHandle {
    let task = runtime.spawn(async move {
        let mut hints = client.new_posts_subscribe();
        reconcile(&client, &handle, &state).await;
        loop {
            let deadline = state.lock().ok().and_then(|state| state.next_deadline());
            let sleep = deadline.map_or_else(
                || tokio::time::sleep(Duration::from_secs(60 * 60)),
                |deadline| tokio::time::sleep_until(TokioInstant::from_std(deadline)),
            );
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                _ = wake.notified() => {}
                hint = hints.recv() => match hint {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!(
                            target: crate::LOG_TARGET,
                            "Rostra notification worker stopped because its wake feed closed"
                        );
                        return;
                    }
                },
            }
            reconcile(&client, &handle, &state).await;
        }
    });
    task.abort_handle()
}

/// Reconciles every replay-complete, loaded agent independently.
async fn reconcile(client: &Client, handle: &tau_client::ClientHandle, state: &Arc<Mutex<State>>) {
    let agents = match state.lock() {
        Ok(state) if state.retry_ready() => state.eligible_agents(),
        Ok(_) | Err(_) => return,
    };
    let mut failure = None;
    for agent_id in agents {
        if let Err(stage) = reconcile_agent(client, handle, state, agent_id.clone()).await {
            failure.get_or_insert((agent_id, stage));
        }
    }
    if let Ok(mut state) = state.lock() {
        finish_reconcile(&mut state, failure);
    }
}

/// Reconciles one bounded feed page and merges it into a pending batch.
async fn reconcile_agent(
    client: &Client,
    handle: &tau_client::ClientHandle,
    state: &Arc<Mutex<State>>,
    agent_id: AgentId,
) -> Result<(), &'static str> {
    let snapshot = match state.lock() {
        Ok(state) => state.scan_snapshot(&agent_id),
        Err(_) => None,
    };
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    if snapshot.inflight_end.is_some() {
        return Ok(());
    }
    let followees = client
        .db()
        .get_self_followees_snapshot()
        .await
        .map_err(|_| "followee snapshot")?;
    let after = snapshot
        .pending
        .as_ref()
        .map_or(snapshot.committed, |pending| pending.end);
    let page = client
        .db()
        .scan_social_post_materializations(
            Some(after),
            NonZeroUsize::new(MATERIALIZATION_PAGE).expect("nonzero fixed page"),
        )
        .await
        .map_err(|_| "materialization scan")?;
    let follows = followees
        .into_iter()
        .map(|followee| (followee.followee, followee))
        .collect::<HashMap<_, _>>();
    let db_init = client.db().db_init_time();
    let page_had_items = !page.items.is_empty();
    let mut count = 0;
    for item in page.items {
        let SocialPostMaterialization::Present {
            post_id,
            authored_at,
            content,
        } = item
        else {
            continue;
        };
        let author = post_id.rostra_id();
        let Some(follow) = follows.get(&author) else {
            continue;
        };
        if !selects_materialization(
            author == client.rostra_id(),
            &authored_at,
            &db_init,
            &follow.first_ts,
            follow
                .persona_selector
                .matches_tags(&content.persona_tags()),
        ) {
            continue;
        }
        count += 1;
    }
    let mut guard = state.lock().map_err(|_| "state lock")?;
    guard
        .merge_page(
            &agent_id,
            &snapshot,
            ScannedPage {
                scanned_through: page.scanned_through,
                had_items: page_had_items,
                exhausted: page.exhausted,
                count,
            },
        )
        .map_err(|_| "checkpoint persistence")?;
    drop(guard);
    report_if_due(state, &agent_id, |report| {
        handle
            .emit_transient_detached(Event::MessageDeliveredReported(report))
            .map_err(|_| "report enqueue")
    })
}

/// Applies one full reconciliation outcome to bounded retry state and
/// diagnostics.
fn finish_reconcile(state: &mut State, failure: Option<(AgentId, &'static str)>) {
    if let Some((agent_id, stage)) = failure {
        state.record_retry();
        if state.should_log_failure() {
            tracing::warn!(
                target: crate::LOG_TARGET,
                agent = %agent_id,
                stage,
                "Rostra notification worker operation failed; retrying with backoff"
            );
        }
    } else {
        state.clear_retry();
    }
}

/// Returns whether an authored timestamp predates either local-history
/// boundary.
fn is_historical<T: Ord>(authored_at: &T, database_initialized: &T, follow_started: &T) -> bool {
    authored_at < database_initialized || authored_at < follow_started
}

/// Selects a materialization only when every receipt and persona gate passes.
fn selects_materialization<T: Ord>(
    is_self: bool,
    authored_at: &T,
    database_initialized: &T,
    follow_started: &T,
    persona_matches: bool,
) -> bool {
    !is_self && !is_historical(authored_at, database_initialized, follow_started) && persona_matches
}

/// Emits only a due report, then waits for the canonical message echo.
fn report_if_due<F>(
    state: &Arc<Mutex<State>>,
    agent_id: &AgentId,
    enqueue: F,
) -> Result<(), &'static str>
where
    F: FnOnce(MessageDelivered<RawMessagePublisherId>) -> Result<(), &'static str>,
{
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return Ok(()),
    };
    if !state.retry_ready() {
        return Ok(());
    }
    let Some((publisher, identity, pending)) = state.due_report(agent_id) else {
        return Ok(());
    };
    let attempt = state
        .allocate_report_attempt()
        .map_err(|_| "report-attempt persistence")?;
    let report = report(
        RawMessagePublisherId::new(publisher.to_string()),
        agent_id.clone(),
        identity.to_string(),
        attempt,
        &pending,
    )
    .map_err(|_| "report construction")?;
    enqueue(report)?;
    state.mark_inflight(agent_id, pending.end);
    Ok(())
}
/// Serializes an opaque value through Tau's existing CBOR bridge.
fn encode_value<T: serde::Serialize>(value: &T) -> Result<CborValue, &'static str> {
    serde_json::to_value(value)
        .map(|value| tau_proto::json_to_cbor(&value))
        .map_err(|_| "notification value cannot be encoded")
}

#[cfg(test)]
#[path = "notification_worker_tests.rs"]
mod notification_worker_tests;
