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
    });
    assert!(timing.activate().is_none());
    assert!(timing.observe().is_none());
}
