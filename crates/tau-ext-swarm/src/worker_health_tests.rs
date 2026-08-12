use std::process::Command;
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

/// A real panic terminates the worker child process, so it cannot continue
/// serving tools with stale publication authority.
#[test]
fn panic_terminates_worker_process() {
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "worker_health_tests::panic_child_terminates",
        ])
        .env("TAU_SWARM_PANIC_CHILD", "1")
        .status()
        .expect("run panicking worker child");

    assert!(!status.success());
}

/// Subprocess-only panic entry point used to verify actual panic termination
/// without failing the parent test process.
#[test]
#[ignore = "invoked by panic_terminates_worker_process in a subprocess"]
fn panic_child_terminates() {
    if std::env::var_os("TAU_SWARM_PANIC_CHILD").is_none() {
        return;
    }
    let health = WorkerHealth::running();
    let _terminal = health.terminal_guard();
    panic!("forced worker panic");
}
