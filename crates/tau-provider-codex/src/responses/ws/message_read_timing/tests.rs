use super::*;

/// Inactive and previous-owner samples must never become current-turn timing,
/// even when an old text message remains in the existing queue.
#[test]
fn observations_are_scoped_to_one_selected_owner() {
    let timing = Arc::new(MessageReadTiming::default());
    assert!(timing.observe().is_none());
    let first = timing.activate().expect("first owner");
    let queued = timing.observe();
    assert!(first.read_at(timing.observe()).is_some());
    assert!(timing.activate().is_none());
    drop(first);
    assert!(timing.observe().is_none());
    let second = timing.activate().expect("second owner");
    assert!(second.read_at(queued).is_none());
    assert!(second.read_at(timing.observe()).is_some());
    drop(second);
    assert!(timing.observe().is_none());
}

/// Generation exhaustion must disable observations rather than reuse an epoch
/// and accidentally attribute a stale queued message to a new owner.
#[test]
fn exhausted_generation_stays_unavailable() {
    let timing = Arc::new(MessageReadTiming {
        generation: AtomicU64::new(u64::MAX - 1),
        controls: Mutex::default(),
    });
    assert!(timing.activate().is_none());
    assert!(timing.observe().is_none());
}

/// Control frames stay bounded and owner-local, without creating application
/// samples or retaining their payloads.
#[test]
fn control_observations_are_scoped_and_separate_from_text() {
    let timing = Arc::new(MessageReadTiming::default());
    timing.observe_control(true);
    let owner = timing.activate().expect("owner");
    assert_eq!(owner.controls().ping_count, 0);
    timing.observe_control(true);
    timing.observe_control(false);
    let controls = owner.controls();
    assert_eq!(controls.ping_count, 1);
    assert_eq!(controls.pong_count, 1);
    assert!(controls.last_ping_at.is_some());
    assert!(controls.last_pong_at.is_some());
    assert!(owner.read_at(None).is_none());
    drop(owner);
    timing.observe_control(true);
    let next = timing.activate().expect("next owner");
    assert_eq!(next.controls().ping_count, 0);
    assert_eq!(next.controls().pong_count, 0);
    assert!(next.controls().last_ping_at.is_none());
}
