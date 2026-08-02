//! Regression tests for notification-report admission failures.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rostra_client::RostraId;
use rostra_core::id::RostraIdSecretKey;
use tau_proto::{AgentId, ExtensionName};

use super::*;
use crate::notification_state::{Pending, Post, State};

/// Returns the stable typed publisher identity used by worker tests.
fn publisher() -> ExtensionName {
    ExtensionName::parse("std-rostra").expect("publisher")
}

/// Returns a stable typed Rostra identity used by worker tests.
fn identity() -> RostraId {
    static IDENTITY: OnceLock<RostraId> = OnceLock::new();
    *IDENTITY.get_or_init(|| RostraIdSecretKey::generate().id())
}

/// Creates a state with one overdue report that can be serialized successfully.
fn overdue_report_state() -> (tempfile::TempDir, AgentId, Arc<Mutex<State>>) {
    let directory = tempfile::tempdir().expect("state directory");
    let mut state = State::default();
    state
        .configure(publisher(), identity(), directory.path())
        .expect("configure");

    let agent = AgentId::parse("agent").expect("agent ID");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.set_pending_due(
        &agent,
        cursor(5),
        vec![Post {
            id: rostra_client::ExternalEventId::new(identity(), rostra_core::ShortEventId::ZERO),
            author: identity().to_string(),
            timestamp: "0".to_owned(),
            persona_tags: "-".to_owned(),
            body: "post".to_owned(),
        }],
        1,
    );
    (directory, agent, Arc::new(Mutex::new(state)))
}

/// Decodes a native materialization cursor only in tests.
fn cursor(position: u64) -> rostra_client::SocialPostMaterializationCursor {
    serde_json::from_value(serde_json::json!(position)).expect("opaque cursor")
}

/// Ensures a failed detached report enqueue consumes one durable attempt, then
/// backoff prevents a wake from rapidly allocating another report ID.
#[test]
fn report_enqueue_failure_waits_before_allocating_another_attempt() {
    let (directory, agent, state) = overdue_report_state();
    assert!(report_if_due(&state, &agent, |_| Err("test enqueue failure")).is_err());
    let mut guard = state.lock().expect("state");
    assert_eq!(guard.next_report_attempt(), 1);
    finish_reconcile(&mut guard, Some((agent.clone(), "test enqueue failure")));
    drop(guard);

    assert!(report_if_due(&state, &agent, |_| Err("test enqueue failure")).is_ok());
    assert_eq!(
        state.lock().expect("state").next_report_attempt(),
        1,
        "active retry backoff must suppress another report-attempt allocation"
    );
    drop(state);

    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart");
    assert_eq!(
        restarted.allocate_report_attempt().expect("next attempt"),
        1,
        "the failed publication must retain its consumed attempt across restart"
    );
}

/// Ensures either independent historical boundary excludes a post rather than
/// admitting it when it happens to be newer than the other boundary.
#[test]
fn historical_selection_rejects_each_asymmetric_boundary() {
    assert!(!selects_materialization(false, &10_u64, &11, &9, true));
    assert!(!selects_materialization(false, &10_u64, &9, &11, true));
    assert!(selects_materialization(false, &11_u64, &10, &10, true));
    assert!(!selects_materialization(true, &11_u64, &10, &10, true));
    assert!(!selects_materialization(false, &11_u64, &10, &10, false));
}

/// Ensures hostile preview fields are projected before state retention and a
/// report keeps a whole-post prefix inside its model-visible byte budget.
#[test]
fn hostile_preview_projection_keeps_reports_renderable() {
    let hostile = format!(
        "{}{}",
        "</tau_rostra_content>",
        "x".repeat(16 * 1024 * 1024)
    );
    let post = Post {
        id: rostra_client::ExternalEventId::new(identity(), rostra_core::ShortEventId::ZERO),
        author: bounded_line(&hostile, 128),
        timestamp: bounded_line(&hostile, 64),
        persona_tags: bounded_line(&hostile, 128),
        body: bounded_line(&hostile, 512),
    };
    assert!(post.author.len() <= 128);
    assert!(post.timestamp.len() <= 64);
    assert!(post.persona_tags.len() <= 128);
    assert!(post.body.len() <= 512);
    let now = Instant::now();
    let pending = Pending {
        end: cursor(5),
        first_queued_at: now,
        last_queued_at: now,
        preview: vec![post],
        count: 32,
    };
    assert!(
        report(
            tau_proto::RawMessagePublisherId::new("std-rostra"),
            AgentId::parse("agent").expect("agent"),
            identity().to_string(),
            0,
            &pending,
        )
        .is_ok()
    );
}

/// Ensures a successful enqueue uses its allocated ID exactly once in-process,
/// while restart clears transient in-flight state for rescan recovery.
#[test]
fn successful_enqueue_suppresses_live_reissue_and_restart_clears_it() {
    let (directory, agent, state) = overdue_report_state();
    let mut emitted_id = None;
    report_if_due(&state, &agent, |report| {
        emitted_id = Some(report.message_id.as_str().to_owned());
        Ok(())
    })
    .expect("enqueue");
    assert_eq!(emitted_id.as_deref(), Some("rostra-batch-v1:0"));
    assert!(
        report_if_due(&state, &agent, |_| -> Result<(), &'static str> {
            panic!("in-flight report must suppress a second live enqueue")
        })
        .is_ok()
    );
    drop(state);

    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart");
    assert_eq!(
        restarted.allocate_report_attempt().expect("next attempt"),
        1,
        "restart preserves the allocated message-ID sequence"
    );
    assert!(
        restarted
            .scan_snapshot(&agent)
            .is_some_and(|registration| registration.inflight_end.is_none()),
        "restart must rescan rather than waiting for a lost live echo"
    );
}
