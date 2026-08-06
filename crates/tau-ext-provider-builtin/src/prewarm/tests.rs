use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tau_provider_codex::TurnAbort;

use super::*;

/// Cancellation keeps ownership until exact completion, preventing
/// duplicate work from racing a still-unwinding transport reservation.
#[test]
fn stale_completion_cannot_remove_successor() {
    let key = PrewarmKey {
        provider: tau_proto::ProviderName::new("chatgpt"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        refresh_id: None,
    };
    let mut supervisor = PrewarmSupervisor::default();
    let (first, _) = supervisor.begin(key.clone()).expect("first begin");
    supervisor.cancel_key(&key);
    assert!(supervisor.begin(key.clone()).is_none());
    supervisor.complete(&key, first);
    let (successor, _) = supervisor.begin(key.clone()).expect("successor begin");
    supervisor.complete(&key, first);
    assert!(
        supervisor.begin(key.clone()).is_none(),
        "stale completion must not retire successor"
    );
    supervisor.complete(&key, successor);
    assert!(supervisor.begin(key).is_some());
}

/// Distinct cache owners cannot create an unbounded number of network
/// threads even when prewarm requests arrive faster than they complete.
#[test]
fn active_work_is_capped_at_pool_capacity() {
    let mut supervisor = PrewarmSupervisor::default();
    for index in 0..tau_provider_codex::MAX_CONCURRENT_PREWARMS {
        assert!(
            supervisor
                .begin(PrewarmKey {
                    provider: tau_proto::ProviderName::new("chatgpt"),
                    agent_id: tau_proto::AgentId::parse(format!("agent-{index}"))
                        .expect("agent id"),
                    refresh_id: None,
                })
                .is_some()
        );
    }
    assert!(
        supervisor
            .begin(PrewarmKey {
                provider: tau_proto::ProviderName::new("chatgpt"),
                agent_id: tau_proto::AgentId::parse("overflow").expect("agent id"),
                refresh_id: None,
            })
            .is_none()
    );
}

/// Directed cancellation removes ownership synchronously and stale worker
/// completion cannot affect a successor.
#[test]
fn refresh_cancel_synchronously_invalidates_generation() {
    let refresh_id =
        tau_proto::ProviderCacheRefreshId::parse("pcr-cancel-test").expect("refresh id");
    let key = PrewarmKey {
        provider: tau_proto::ProviderName::new("chatgpt"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent"),
        refresh_id: Some(refresh_id.clone()),
    };
    let mut supervisor = PrewarmSupervisor::default();
    let (generation, _) = supervisor.begin(key.clone()).expect("begin");
    supervisor.cancel_refresh(&refresh_id);
    assert!(supervisor.is_empty());
    let (successor, _) = supervisor.begin(key.clone()).expect("successor");
    supervisor.complete(&key, generation);
    assert!(!supervisor.is_empty());
    supervisor.complete(&key, successor);
    assert!(supervisor.is_empty());
}

/// Shared cooldown evidence wakes matching refresh transport while retaining
/// ownership for its eventual terminal.
#[test]
fn provider_cooldown_cancels_transport_but_retains_owner() {
    let key = PrewarmKey {
        provider: tau_proto::ProviderName::new("chatgpt"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent"),
        refresh_id: Some(
            tau_proto::ProviderCacheRefreshId::parse("pcr-cooldown").expect("refresh id"),
        ),
    };
    let mut supervisor = PrewarmSupervisor::default();
    let (generation, mut abort) = supervisor.begin(key.clone()).expect("begin");
    supervisor.cancel_provider(&tau_proto::ProviderName::new("chatgpt"));
    assert!(abort.is_aborted());
    assert!(!supervisor.is_empty());
    supervisor.complete(&key, generation);
    assert!(
        supervisor.is_empty(),
        "the canceled cooldown worker releases ownership only at exact completion"
    );
}

/// Once cancellation enters a registered callback, guard drop must wait for
/// that callback to finish; this makes unregister the publication boundary.
#[test]
fn cancellation_callback_finishes_before_guard_unregisters() {
    let mut abort = PrewarmAbort::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let callback_done = Arc::new(AtomicBool::new(false));
    let callback_done_from_waker = Arc::clone(&callback_done);
    let guard = abort.register_waker(Arc::new(move || {
        entered_tx.send(()).expect("callback entered");
        release_rx
            .lock()
            .expect("release receiver")
            .recv()
            .expect("callback release");
        callback_done_from_waker.store(true, Ordering::Release);
    }));
    let cancel_abort = abort.clone();
    let cancel = thread::spawn(move || cancel_abort.cancel());
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancel callback starts");
    assert!(
        abort.callback_registry_is_locked(),
        "active cancellation callback must retain the registry lock"
    );
    let callback_done_after_drop = Arc::clone(&callback_done);
    let dropper = thread::spawn(move || {
        drop(guard);
        assert!(
            callback_done_after_drop.load(Ordering::Acquire),
            "guard drop returned before cancellation callback completed"
        );
    });
    release_tx.send(()).expect("release callback");
    cancel.join().expect("cancel thread");
    dropper.join().expect("guard drop thread");
}

/// If callback unregistration completes first, later cancellation starts
/// after the worker's logical publication boundary and cannot call it.
#[test]
fn unregistered_callback_is_not_called_by_later_cancellation() {
    let mut abort = PrewarmAbort::default();
    let calls = Arc::new(AtomicBool::new(false));
    let callback_calls = Arc::clone(&calls);
    let guard = abort.register_waker(Arc::new(move || {
        callback_calls.store(true, Ordering::Release);
    }));
    drop(guard);
    abort.cancel();
    assert!(!calls.load(Ordering::Acquire));
    assert!(abort.is_aborted());
}
