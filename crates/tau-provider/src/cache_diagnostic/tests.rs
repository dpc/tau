use super::*;

/// In-flight reservations count against both approved ceilings, and loss
/// creates a sequence hole rather than reusing correlation.
#[test]
fn admission_includes_in_flight_and_counts_loss() {
    let budget = Box::leak(Box::new(Budget::new()));
    let held: Vec<_> = (0..MAX_RECORDS)
        .map(|_| budget.reserve().expect("reservation below bound"))
        .collect();
    assert_eq!(held.len() * MAX_RECORD_BYTES, 16 * 1024 * 1024);
    assert!(budget.reserve().is_none());
    drop(held);
    let next = budget.reserve().expect("released capacity");
    assert_eq!(next.sequence(), 65);
    assert_eq!(next.dropped_records_total(), 65);
}

/// Sequence exhaustion cannot wrap and accidentally merge process records.
#[test]
fn exhausted_sequence_stays_disabled() {
    let budget = Box::leak(Box::new(Budget::new()));
    budget.sequence.store(u64::MAX, Ordering::Relaxed);
    assert!(budget.reserve().is_none());
    assert!(budget.reserve().is_none());
}

/// Successful worker delivery releases its slot without claiming filesystem
/// persistence or increasing the known-loss counter.
#[test]
fn delivered_reservation_releases_without_loss() {
    let budget = Box::leak(Box::new(Budget::new()));
    let mut reservation = budget.reserve().expect("empty budget");
    reservation.delivered();
    drop(reservation);
    assert_eq!(budget.reserved.load(Ordering::Relaxed), 0);
    assert_eq!(budget.dropped.load(Ordering::Relaxed), 0);
}
