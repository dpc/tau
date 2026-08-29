use std::sync::mpsc;

use crate::worker_health::WorkerHealth;

/// A normally terminating worker retires its publication authority before its
/// join can complete.
#[test]
fn terminal_return_marks_health_indeterminate() {
    let health = WorkerHealth::running();
    let task_health = health.clone();
    let thread = std::thread::spawn(move || {
        let _terminal = task_health.terminal_guard();
    });

    thread.join().expect("worker return");

    assert!(!health.is_running());
}

/// Retirement cannot pass a mutation holding publication authority; once that
/// mutation releases authority, retirement completes and later admission fails.
#[test]
fn retirement_serializes_with_complete_mutation_authority() {
    let health = WorkerHealth::running();
    let authority = health.mutation_authority().expect("running authority");
    let (closed_tx, closed_rx) = mpsc::channel();
    let (drained_tx, drained_rx) = mpsc::channel();
    let (retired_tx, retired_rx) = mpsc::channel();
    let retirement_health = health.clone();
    let retirement = std::thread::spawn(move || {
        drop(retirement_health.terminal_guard_notifying(closed_tx, drained_tx));
        retired_tx.send(()).expect("retirement signal");
    });

    closed_rx.recv().expect("admission close");
    assert!(retired_rx.try_recv().is_err());
    assert!(drained_rx.try_recv().is_err());
    assert!(
        health.mutation_authority().is_err(),
        "retirement must close later admission before draining"
    );
    drop(authority);
    drained_rx.recv().expect("completed mutation drain");
    retired_rx.recv().expect("retirement after mutation");
    retirement.join().expect("retirement thread");
    assert!(health.mutation_authority().is_err());
}

/// A worker panic unwind retires publication health, preventing later mutation
/// admission after the worker can no longer publish.
#[test]
fn panic_unwind_retires_worker_health() {
    let health = WorkerHealth::running();
    let task_health = health.clone();
    let worker = std::thread::spawn(move || {
        let _terminal = task_health.terminal_guard();
        panic!("forced worker panic");
    });

    assert!(
        worker.join().is_err(),
        "worker panic must be contained by its owned thread"
    );
    assert!(!health.is_running());
    assert!(health.mutation_authority().is_err());
}
