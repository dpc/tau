use super::{AdmissionQueue, CAPACITY, ReserveError};

/// Reservations bound queued plus pre-ACK work and release capacity on failure.
#[test]
fn reservation_bounds_and_releases_capacity() {
    let queue = AdmissionQueue::<()>::new();
    let reservations = (0..CAPACITY)
        .map(|_| queue.reserve().expect("capacity"))
        .collect::<Vec<_>>();
    assert!(matches!(queue.reserve(), Err(ReserveError::Full)));
    drop(reservations);
    queue.reserve().expect("released capacity");
}

/// Dequeued work remains part of the 64-occurrence bound until its processor
/// reaches a terminal outcome, preventing a slow API call from opening
/// capacity.
#[test]
fn in_flight_work_retains_capacity() {
    let queue = AdmissionQueue::new();
    queue.reserve().expect("test queue operation").commit(0);
    let (_item, in_flight) = queue.pop().expect("test queue operation");
    let reservations = (1..CAPACITY)
        .map(|_| queue.reserve().expect("remaining capacity"))
        .collect::<Vec<_>>();
    assert!(matches!(queue.reserve(), Err(ReserveError::Full)));
    drop(in_flight);
    queue.reserve().expect("terminal outcome releases capacity");
    drop(reservations);
}

/// Committing reservations preserves commit order for the sole serial consumer.
#[test]
fn committed_work_is_fifo() {
    let queue = AdmissionQueue::new();
    queue.reserve().expect("test queue operation").commit(1);
    queue.reserve().expect("test queue operation").commit(2);
    let (first, first_permit) = queue.pop().expect("test queue operation");
    assert_eq!(first, 1);
    drop(first_permit);
    let (second, _second_permit) = queue.pop().expect("test queue operation");
    assert_eq!(second, 2);
}

/// Closure rejects new work while allowing already acknowledged work to drain.
#[test]
fn closure_drains_committed_work() {
    let queue = AdmissionQueue::new();
    queue.reserve().expect("test queue operation").commit(1);
    queue.close();
    assert!(matches!(queue.reserve(), Err(ReserveError::Closed)));
    let (item, permit) = queue.pop().expect("test queue operation");
    assert_eq!(item, 1);
    drop(permit);
    assert!(queue.pop().is_none());
}
