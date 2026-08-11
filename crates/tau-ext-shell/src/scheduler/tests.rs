use std::time as path_std_time;

use super::*;

fn test_meta(call_id: &str) -> WorkMeta {
    WorkMeta {
        call_id: Some(call_id.into()),
        agent_id: Some(AgentId::parse("agent-a").expect("agent id")),
        queued_bytes: 1,
    }
}

/// Ensures bounded queue overflow produces a clear backpressure error
/// instead of spawning extra threads.
#[test]
fn enqueue_respects_total_limit() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        total_limit: 1,
        control_workers: 0,
        user_workers: 0,
        general_workers: 0,
        ..SchedulerConfig::default()
    });

    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-a"), || {})
        .expect("first queued");
    let err = scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-b"), || {})
        .expect_err("second should hit total limit");

    assert!(err.message.contains("queue limit is 1"));
}

/// Ensures scheduler handoff cleanup removes queued work before it can run;
/// lifecycle cancellation owns terminal reporting separately.
#[test]
fn cancel_queued_call_removes_work() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        general_workers: 0,
        ..SchedulerConfig::default()
    });
    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-a"), || {
            panic!("must not run")
        })
        .expect("queued");

    assert!(scheduler.cancel_queued_call(&"call-a".into()));
}

/// Ensures lifecycle cleanup removes queued work before an unloaded agent
/// can later run it.
#[test]
fn cancel_agent_removes_owned_queued_work() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 0,
        ..SchedulerConfig::default()
    });
    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-a"), || {
            panic!("must not run")
        })
        .expect("queued");

    assert_eq!(
        scheduler.cancel_agent(&AgentId::parse("agent-a").expect("agent id")),
        1
    );
    assert!(!scheduler.cancel_queued_call(&"call-a".into()));
}

/// Ensures the approximate queued-argument byte budget is enforced before
/// accepting more work.
#[test]
fn enqueue_respects_queued_byte_limit() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        queued_bytes_limit: 1,
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 0,
        ..SchedulerConfig::default()
    });
    let err = scheduler
        .enqueue(
            WorkPriority::Bulk,
            WorkMeta {
                queued_bytes: 2,
                ..test_meta("call-a")
            },
            || {},
        )
        .expect_err("oversized queued arguments should fail");

    assert!(err.message.contains("queued shell tool arguments exceed"));
}

/// Ensures control work has a dedicated lane and worker so it can proceed
/// while a bulk worker is occupied.
#[test]
fn control_work_runs_while_bulk_worker_is_busy() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        control_workers: 1,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 1,
        ..SchedulerConfig::default()
    });
    let (bulk_started_tx, bulk_started_rx) = mpsc::channel();
    let (release_bulk_tx, release_bulk_rx) = mpsc::channel();
    let (control_ran_tx, control_ran_rx) = mpsc::channel();

    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-bulk"), move || {
            bulk_started_tx.send(()).expect("bulk started");
            release_bulk_rx.recv().expect("release bulk");
        })
        .expect("bulk queued");
    bulk_started_rx.recv().expect("bulk worker started");

    scheduler
        .enqueue(
            WorkPriority::Control,
            test_meta("call-control"),
            move || {
                control_ran_tx.send(()).expect("control ran");
            },
        )
        .expect("control queued");

    control_ran_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("control worker should not be starved by bulk work");
    release_bulk_tx.send(()).expect("release bulk");
}

/// Ensures scheduler drop is a deterministic lifecycle boundary: queued
/// work is discarded, shutdown wakes workers, and drop waits for already
/// running work before returning.
#[test]
fn drop_cancels_queued_work_and_joins_running_workers() {
    let scheduler = WorkScheduler::new(SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 1,
        ..SchedulerConfig::default()
    });
    let (running_tx, running_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (drop_done_tx, drop_done_rx) = mpsc::channel();
    let (queued_drop_tx, queued_drop_rx) = mpsc::channel();
    struct NotifyOnDrop(mpsc::Sender<()>);
    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-running"), move || {
            running_tx.send(()).expect("running marker");
            release_rx.recv().expect("release running work");
        })
        .expect("running work queued");
    running_rx.recv().expect("worker started");
    let queued_drop = NotifyOnDrop(queued_drop_tx);
    scheduler
        .enqueue(WorkPriority::Bulk, test_meta("call-queued"), move || {
            let _queued_drop = queued_drop;
            panic!("queued work must be cancelled on drop")
        })
        .expect("queued work accepted");

    let dropper = std::thread::spawn(move || {
        drop(scheduler);
        drop_done_tx.send(()).expect("drop done marker");
    });

    queued_drop_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("queued work should be dropped during scheduler drop");
    assert!(
        drop_done_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "drop must wait for the running worker to finish"
    );
    release_tx.send(()).expect("release running work");
    drop_done_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("drop should finish after running worker exits");
    dropper.join().expect("dropper thread");
}
