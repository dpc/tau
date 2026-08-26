use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use super::*;

const PRE_TRIGGER_WAIT: Duration = Duration::from_millis(50);
const RESULT_WAIT: Duration = Duration::from_secs(1);
const CONCURRENT_SENDERS: usize = 8;

/// Ensures dropping the receiver does not make later sender notifications
/// panic.
#[test]
fn notify_after_receiver_drop_returns_normally() {
    let (tx, rx) = channel();
    drop(rx);
    tx.notify();
}

/// Ensures burst notifications coalesce into one pending wakeup instead of
/// queuing.
#[test]
fn multiple_notifies_coalesce() {
    let (tx, rx) = channel();
    tx.notify();
    tx.notify();
    tx.notify();
    assert_eq!(rx.recv(), Ok(()));
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Empty));
}

/// Ensures `try_recv` reports an idle connected channel without blocking.
#[test]
fn try_recv_returns_empty_when_not_notified() {
    let (_tx, rx) = channel();
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Empty));
}

/// Ensures `try_recv` reports and consumes exactly one pending notification,
/// then resets the flag.
#[test]
fn try_recv_returns_notified_and_resets() {
    let (tx, rx) = channel();
    tx.notify();
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Notified));
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Empty));
}

/// Ensures the public auto-trait contract stays single-consumer: movable to a
/// worker thread, but not shareable by reference across threads.
#[test]
fn receiver_auto_traits_match_single_consumer_contract() {
    static_assertions::assert_impl_all!(Receiver: Send);
    static_assertions::assert_not_impl_any!(Receiver: Sync);
}

/// Ensures `recv` waits for a later notification rather than returning early.
#[test]
fn recv_blocks_until_notified() {
    let (tx, rx) = channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        ready_tx.send(()).expect("receiver readiness sent");
        result_tx.send(rx.recv()).expect("receiver result sent");
    });

    assert_eq!(ready_rx.recv_timeout(RESULT_WAIT), Ok(()));
    assert_eq!(
        result_rx.recv_timeout(PRE_TRIGGER_WAIT),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    tx.notify();
    assert_eq!(result_rx.recv_timeout(RESULT_WAIT), Ok(Ok(())));
    handle.join().expect("receiver thread panicked");
}

/// Ensures timed receive consumes an already pending wake without consulting
/// wall-clock delay.
#[test]
fn recv_timeout_consumes_pending_notification() {
    let (tx, rx) = channel();
    tx.notify();
    assert_eq!(rx.recv_timeout(Duration::ZERO), Ok(()));
    assert_eq!(
        rx.recv_timeout(Duration::ZERO),
        Err(RecvTimeoutError::Timeout)
    );
}

/// Ensures a duration beyond the platform's finite `Instant` range behaves as
/// an interruptible unbounded wait instead of panicking during deadline math.
#[test]
fn recv_timeout_accepts_duration_beyond_instant_range() {
    let (tx, rx) = channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        ready_tx.send(()).expect("receiver readiness sent");
        result_tx
            .send(rx.recv_timeout(Duration::MAX))
            .expect("receiver result sent");
    });

    assert_eq!(ready_rx.recv_timeout(RESULT_WAIT), Ok(()));
    assert_eq!(
        result_rx.recv_timeout(PRE_TRIGGER_WAIT),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    tx.notify();
    assert_eq!(result_rx.recv_timeout(RESULT_WAIT), Ok(Ok(())));
    handle.join().expect("receiver thread panicked");
}

/// Ensures timed receive preserves pending-notification priority over channel
/// disconnection.
#[test]
fn recv_timeout_delivers_pending_notification_before_disconnect() {
    let (tx, rx) = channel();
    tx.notify();
    drop(tx);
    assert_eq!(rx.recv_timeout(Duration::ZERO), Ok(()));
    assert_eq!(
        rx.recv_timeout(Duration::ZERO),
        Err(RecvTimeoutError::Disconnected)
    );
}

/// Ensures a timed waiter wakes promptly when a barrier-released producer
/// notifies it rather than sleeping until its deadline.
#[test]
fn recv_timeout_wakes_on_notification() {
    let (tx, rx) = channel();
    let start = Arc::new(Barrier::new(2));
    let sender_start = Arc::clone(&start);
    let sender = thread::spawn(move || {
        sender_start.wait();
        tx.notify();
    });
    start.wait();
    assert_eq!(rx.recv_timeout(RESULT_WAIT), Ok(()));
    sender.join().expect("sender thread panicked");
}

/// Ensures a barrier-aligned sender cohort coalesces concurrent notifications
/// and disconnects only after every clone drops.
#[test]
fn multiple_senders() {
    let (tx, rx) = channel();
    let start = Arc::new(Barrier::new(CONCURRENT_SENDERS + 1));
    let senders: Vec<_> = (0..CONCURRENT_SENDERS).map(|_| tx.clone()).collect();
    drop(tx);

    let workers: Vec<_> = senders
        .into_iter()
        .map(|tx| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                tx.notify();
            })
        })
        .collect();
    start.wait();

    for worker in workers {
        worker.join().expect("sender thread panicked");
    }

    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Notified));
    assert_eq!(rx.try_recv(), Err(Disconnected));
}

/// Ensures `recv` reports disconnect once the original sender is dropped.
#[test]
fn disconnect_after_all_senders_dropped() {
    let (tx, rx) = channel();
    drop(tx);
    assert_eq!(rx.recv(), Err(Disconnected));
}

/// Ensures the channel remains connected until every sender clone is dropped.
#[test]
fn disconnect_after_last_clone_dropped() {
    let (tx, rx) = channel();
    let tx2 = tx.clone();
    drop(tx);
    // Still one sender alive.
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Empty));
    drop(tx2);
    assert_eq!(rx.recv(), Err(Disconnected));
}

/// Ensures `try_recv` drains a pending notification before reporting
/// disconnect.
#[test]
fn try_recv_delivers_pending_notification_before_disconnect() {
    let (tx, rx) = channel();
    tx.notify();
    drop(tx);
    assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Notified));
    assert_eq!(rx.try_recv(), Err(Disconnected));
}

/// Ensures `recv` delivers a pending notification before reporting disconnect.
#[test]
fn notification_takes_priority_over_disconnect() {
    let (tx, rx) = channel();
    tx.notify();
    drop(tx);
    // Notification delivered first despite disconnect.
    assert_eq!(rx.recv(), Ok(()));
    // Now disconnected.
    assert_eq!(rx.recv(), Err(Disconnected));
}

/// Ensures a blocked `recv` wakes and reports disconnect when the last sender
/// drops.
#[test]
fn recv_unblocks_on_disconnect() {
    let (tx, rx) = channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        ready_tx.send(()).expect("receiver readiness sent");
        result_tx.send(rx.recv()).expect("receiver result sent");
    });

    assert_eq!(ready_rx.recv_timeout(RESULT_WAIT), Ok(()));
    assert_eq!(
        result_rx.recv_timeout(PRE_TRIGGER_WAIT),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    drop(tx);
    assert_eq!(result_rx.recv_timeout(RESULT_WAIT), Ok(Err(Disconnected)));
    handle.join().expect("receiver thread panicked");
}
