use super::*;

/// Ensures provider waiting retains the approved thirty-second absolute cap.
#[test]
fn provider_wait_uses_absolute_cap() {
    let start = Instant::now();
    let deadline = SessionInitDeadline::new(start);

    assert_eq!(deadline.next_deadline(), start + ABSOLUTE_TIMEOUT);
    assert!(!deadline.expired(start + Duration::from_secs(2)));
    assert!(deadline.expired(start + ABSOLUTE_TIMEOUT));
}
