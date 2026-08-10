use super::*;

/// Ensures accepted provider progress renews the idle deadline.
#[test]
fn accepted_progress_renews_idle_deadline() {
    let start = Instant::now();
    let mut progress = SessionInitProgressGeneration::default();
    let mut deadline = SessionInitDeadline::new(start, progress);

    progress.advance();
    deadline.observe_progress(start + Duration::from_millis(1_500), progress);

    assert!(!deadline.expired(start + Duration::from_secs(3)));
    assert!(deadline.expired(start + Duration::from_millis(3_500)));
}

/// Ensures unrelated events cannot keep session initialization alive.
#[test]
fn unchanged_progress_does_not_renew_idle_deadline() {
    let start = Instant::now();
    let progress = SessionInitProgressGeneration::default();
    let mut deadline = SessionInitDeadline::new(start, progress);

    deadline.observe_progress(start + Duration::from_millis(1_500), progress);

    assert!(deadline.expired(start + Duration::from_secs(2)));
}

/// Ensures repeated accepted progress cannot extend provider waiting past
/// the approved thirty-second absolute cap.
#[test]
fn progress_cannot_renew_past_absolute_cap() {
    let start = Instant::now();
    let mut progress = SessionInitProgressGeneration::default();
    let mut deadline = SessionInitDeadline::new(start, progress);
    for generation in 1..=20 {
        progress.advance();
        deadline.observe_progress(start + Duration::from_millis(1_500 * generation), progress);
    }

    assert_eq!(deadline.next_deadline(), start + ABSOLUTE_TIMEOUT);
    assert!(deadline.expired(start + ABSOLUTE_TIMEOUT));
}
