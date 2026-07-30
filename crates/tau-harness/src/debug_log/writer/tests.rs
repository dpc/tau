use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic as path_std_sync_atomic;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{io as path_std_io, sync as path_std_sync};

use fs2::FileExt as _;

use super::DebugWriter;
use crate::debug_log::test_io::AppendFault;

fn queue_state() -> super::DebugWriterQueue {
    super::DebugWriterQueue {
        retained_lines: path_std_sync_atomic::AtomicUsize::new(0),
        retained_bytes: path_std_sync_atomic::AtomicUsize::new(0),
        poisoned: path_std_sync_atomic::AtomicBool::new(false),
        drop_episode: path_std_sync_atomic::AtomicU64::new(0),
        io_failures: path_std_sync_atomic::AtomicU64::new(0),
        io_warning_active: path_std_sync_atomic::AtomicBool::new(false),
        lock_attempts: path_std_sync_atomic::AtomicUsize::new(0),
        completed_jobs: path_std_sync_atomic::AtomicUsize::new(0),
        worker_traces: path_std_sync::Mutex::new(Vec::new()),
        test_pauses: super::DebugWriterTestPauses::default(),
    }
}

fn wait_for_line_count(path: &Path, expected: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let lines = raw
                .lines()
                .filter(|line| !line.is_empty())
                .map(serde_json::from_str)
                .collect::<Result<Vec<_>, _>>();
            if let Ok(lines) = lines
                && lines.len() == expected
            {
                return lines;
            }
        }
        assert!(
            Instant::now() < deadline,
            "debug writer did not produce {expected} lines"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_atomic(value: &path_std_sync::atomic::AtomicUsize, expected: usize, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while value.load(Ordering::Acquire) != expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::yield_now();
    }
}

/// Drop episodes count once per line, saturate, and reset only on recovery.
#[test]
fn drop_warning_episode_counts_saturates_and_recovers() {
    let queue = queue_state();
    queue.note_drop();
    assert_eq!(
        queue.drop_episode.load(Ordering::Acquire),
        super::DROP_EPISODE_ACTIVE | 1
    );
    queue.note_drop();
    assert_eq!(
        queue.drop_episode.load(Ordering::Acquire),
        super::DROP_EPISODE_ACTIVE | 2
    );
    queue.drop_episode.store(
        super::DROP_EPISODE_ACTIVE | super::DROP_EPISODE_COUNT_MASK,
        Ordering::Release,
    );
    queue.note_drop();
    assert_eq!(
        queue.drop_episode.load(Ordering::Acquire),
        super::DROP_EPISODE_ACTIVE | super::DROP_EPISODE_COUNT_MASK
    );
    queue.note_recovered();
    assert_eq!(queue.drop_episode.load(Ordering::Acquire), 0);
}

/// Recoverable I/O warning episodes coalesce, saturate, bound text, and reset.
#[test]
fn io_warning_episode_counts_saturates_bounds_and_recovers() {
    let queue = queue_state();
    let long_error = path_std_io::Error::other("x".repeat(super::DIAGNOSTIC_CHARS * 2));
    assert!(super::bounded_diagnostic(&long_error).chars().count() <= super::DIAGNOSTIC_CHARS + 1);
    queue.note_io_failure(&long_error);
    assert_eq!(queue.io_failures.load(Ordering::Acquire), 1);
    assert!(queue.io_warning_active.load(Ordering::Acquire));
    queue.note_io_failure(&long_error);
    assert_eq!(queue.io_failures.load(Ordering::Acquire), 2);
    assert!(queue.io_warning_active.load(Ordering::Acquire));
    queue.io_failures.store(u64::MAX, Ordering::Release);
    queue.note_io_failure(&long_error);
    assert_eq!(queue.io_failures.load(Ordering::Acquire), u64::MAX);
    queue.note_recovered();
    assert_eq!(queue.io_failures.load(Ordering::Acquire), 0);
    assert!(!queue.io_warning_active.load(Ordering::Acquire));
}

/// A contended sidecar blocks only the detached worker, not admission.
#[test]
fn admission_does_not_wait_for_the_sidecar_lock() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = path_std_sync::Arc::new(DebugWriter::start());
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    let producer = path_std_sync::Arc::clone(&writer);
    let producer_path = path.clone();
    let (admitted_tx, admitted_rx) = path_std_sync::mpsc::channel();
    let producer_thread = std::thread::spawn(move || {
        let accepted = producer.enqueue(producer_path, b"{\"sequence\":1}\n".to_vec());
        admitted_tx.send(accepted).expect("report admission");
    });
    assert!(
        admitted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("admission blocked on sidecar I/O")
    );
    fs2::FileExt::unlock(&lock).expect("release sidecar");
    producer_thread.join().expect("producer thread");
    let lines = wait_for_line_count(&path, 1);
    assert_eq!(lines[0]["sequence"], 1);
}

/// One detached worker preserves admission order for accepted lines.
#[test]
fn preserves_accepted_line_order() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    for sequence in 0..3 {
        assert!(writer.enqueue(
            path.clone(),
            format!("{{\"sequence\":{sequence}}}\n").into_bytes(),
        ));
    }
    let lines = wait_for_line_count(&path, 3);
    assert_eq!(
        lines
            .iter()
            .map(|line| line["sequence"].as_u64().expect("sequence"))
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

/// The exact retained-line bound drops only the overflowing line and
/// drains.
#[test]
fn retained_line_bound_drops_only_overflow() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    assert!(writer.enqueue(path.clone(), b"{\"sequence\":0}\n".to_vec()));
    wait_for_atomic(&writer.queue.lock_attempts, 1, "in-flight lock attempt");
    for sequence in 1..super::MAX_RETAINED_LINES {
        assert!(writer.enqueue(
            path.clone(),
            format!("{{\"sequence\":{sequence}}}\n").into_bytes(),
        ));
    }
    assert!(!writer.enqueue(path.clone(), b"{\"overflow\":true}\n".to_vec()));
    fs2::FileExt::unlock(&lock).expect("release sidecar");
    let lines = wait_for_line_count(&path, super::MAX_RETAINED_LINES);
    assert!(lines.iter().all(|line| line.get("overflow").is_none()));
    wait_for_atomic(&writer.queue.retained_lines, 0, "line permits");
    wait_for_atomic(&writer.queue.retained_bytes, 0, "byte permits");
    assert!(writer.enqueue(path.clone(), b"{\"recovered\":true}\n".to_vec()));
    let lines = wait_for_line_count(&path, super::MAX_RETAINED_LINES + 1);
    assert_eq!(lines.last().expect("recovery line")["recovered"], true);
}

/// The exact byte cap includes one in-flight line and its destination path.
#[test]
fn retained_byte_bound_includes_line_and_path() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    let exact_line = vec![b'x'; super::MAX_RETAINED_BYTES - path.as_os_str().len()];
    let exact_line_len = exact_line.len() as u64;
    assert!(writer.enqueue(path.clone(), exact_line));
    wait_for_atomic(&writer.queue.lock_attempts, 1, "byte-bound lock attempt");
    assert!(!writer.enqueue(path.clone(), b"x".to_vec()));
    fs2::FileExt::unlock(&lock).expect("release sidecar");
    wait_for_atomic(&writer.queue.completed_jobs, 1, "byte-bound completion");
    wait_for_atomic(&writer.queue.retained_lines, 0, "line permit recovery");
    wait_for_atomic(&writer.queue.retained_bytes, 0, "byte permit recovery");
    assert_eq!(
        std::fs::metadata(path).expect("debug log metadata").len(),
        exact_line_len
    );
    assert_eq!(writer.queue.retained_lines.load(Ordering::Acquire), 0);
    assert_eq!(writer.queue.retained_bytes.load(Ordering::Acquire), 0);
}

/// A cached handle still reacquires the per-line sidecar before line two.
#[test]
fn cached_handle_reacquires_sidecar_for_every_line() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    assert!(writer.enqueue(path.clone(), b"{\"sequence\":1}\n".to_vec()));
    wait_for_line_count(&path, 1);

    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    assert!(writer.enqueue(path.clone(), b"{\"sequence\":2}\n".to_vec()));
    wait_for_atomic(&writer.queue.lock_attempts, 2, "second lock attempt");
    assert_eq!(writer.queue.completed_jobs.load(Ordering::Acquire), 1);
    assert_eq!(wait_for_line_count(&path, 1)[0]["sequence"], 1);
    fs2::FileExt::unlock(&lock).expect("release sidecar");
    let lines = wait_for_line_count(&path, 2);
    assert_eq!(lines[1]["sequence"], 2);
}

/// Path switching preserves FIFO order within each destination.
#[test]
fn switches_cached_paths_without_reordering() {
    let td = tempfile::tempdir().expect("tempdir");
    let first = td.path().join("first/events.jsonl");
    let second = td.path().join("second/events.jsonl");
    let writer = DebugWriter::start();
    assert!(writer.enqueue(first.clone(), b"{\"sequence\":1}\n".to_vec()));
    assert!(writer.enqueue(second.clone(), b"{\"sequence\":2}\n".to_vec()));
    assert!(writer.enqueue(first.clone(), b"{\"sequence\":3}\n".to_vec()));
    let first_lines = wait_for_line_count(&first, 2);
    let second_lines = wait_for_line_count(&second, 1);
    assert_eq!(first_lines[0]["sequence"], 1);
    assert_eq!(second_lines[0]["sequence"], 2);
    assert_eq!(first_lines[1]["sequence"], 3);
}

/// A rolled-back worker failure omits one line and permits later retry.
#[test]
fn recoverable_append_failure_omits_line_and_retries() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    assert!(writer.enqueue(path.clone(), b"{\"sequence\":1}\n".to_vec()));
    wait_for_line_count(&path, 1);
    assert!(writer.enqueue_fault(
        path.clone(),
        b"{\"sequence\":2}\n".to_vec(),
        AppendFault {
            fail_write_at: Some(1),
            ..AppendFault::default()
        },
    ));
    assert!(writer.enqueue(path.clone(), b"{\"sequence\":3}\n".to_vec()));
    let lines = wait_for_line_count(&path, 2);
    assert_eq!(lines[0]["sequence"], 1);
    assert_eq!(lines[1]["sequence"], 3);
}

/// Uncertain rollback poisons the worker and rejects every later line.
#[test]
fn uncertain_rollback_poison_rejects_later_lines() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = DebugWriter::start();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    assert!(writer.enqueue_fault(
        path.clone(),
        b"{\"sequence\":1}\n".to_vec(),
        AppendFault {
            fail_write_at: Some(1),
            fail_truncate: true,
            ..AppendFault::default()
        },
    ));
    for sequence in 2..=8 {
        assert!(writer.enqueue(
            path.clone(),
            format!("{{\"sequence\":{sequence}}}\n").into_bytes(),
        ));
    }
    fs2::FileExt::unlock(&lock).expect("release sidecar");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !writer.queue.poisoned.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "worker did not publish poison");
        std::thread::yield_now();
    }
    wait_for_atomic(&writer.queue.retained_lines, 0, "poison queue drain");
    wait_for_atomic(&writer.queue.retained_bytes, 0, "poison byte drain");
    let raw = std::fs::read_to_string(&path).expect("read poisoned debug log");
    for sequence in 2..=8 {
        assert!(
            !raw.contains(&format!("\"sequence\":{sequence}")),
            "queued follower {sequence} reached the file after poison"
        );
    }
    assert!(!writer.enqueue(path, b"{\"sequence\":2}\n".to_vec()));
}

/// A job admitted after poison drains but before receiver destruction releases
/// its exact line and byte accounting when receiver destruction drops it.
#[test]
fn poison_receiver_drop_releases_racing_admission_accounting() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("events.jsonl");
    let writer = path_std_sync::Arc::new(DebugWriter::start());
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(td.path().join("events.jsonl.lock"))
        .expect("open sidecar");
    lock.lock_exclusive().expect("hold sidecar");
    assert!(writer.enqueue_fault(
        path.clone(),
        b"{\"sequence\":1}\n".to_vec(),
        AppendFault {
            fail_write_at: Some(1),
            fail_truncate: true,
            ..AppendFault::default()
        },
    ));
    wait_for_atomic(&writer.queue.lock_attempts, 1, "poisoning lock attempt");

    let before_send = super::DebugWriterTestPause::new();
    *writer
        .queue
        .test_pauses
        .before_send
        .lock()
        .expect("before-send test pause mutex poisoned") = Some(before_send.clone());
    let after_poison_drain = super::DebugWriterTestPause::new();
    *writer
        .queue
        .test_pauses
        .after_poison_drain
        .lock()
        .expect("after-poison-drain test pause mutex poisoned") = Some(after_poison_drain.clone());

    let producer = path_std_sync::Arc::clone(&writer);
    let producer_path = path.clone();
    let producer_thread =
        std::thread::spawn(move || producer.enqueue(producer_path, b"{\"sequence\":2}\n".to_vec()));
    before_send.entered.wait();

    fs2::FileExt::unlock(&lock).expect("release sidecar");
    after_poison_drain.entered.wait();
    before_send.resume.wait();
    assert!(
        producer_thread.join().expect("producer thread"),
        "receiver still exists, so the racing admission must reach its queue"
    );
    assert_eq!(writer.queue.retained_lines.load(Ordering::Acquire), 1);
    assert_ne!(writer.queue.retained_bytes.load(Ordering::Acquire), 0);

    after_poison_drain.resume.wait();
    wait_for_atomic(
        &writer.queue.retained_lines,
        0,
        "receiver-drop line permit release",
    );
    wait_for_atomic(
        &writer.queue.retained_bytes,
        0,
        "receiver-drop byte permit release",
    );
    assert!(writer.queue.poisoned.load(Ordering::Acquire));
}

/// Directory, sidecar-open, lock, and append-open failures all emit complete
/// worker timing fields, including the slow-cycle classification.
#[test]
fn early_io_failures_trace_complete_worker_cycles() {
    let td = tempfile::tempdir().expect("tempdir");
    let writer = DebugWriter::start();
    let stages = [
        super::DebugWriterIoStage::Directory,
        super::DebugWriterIoStage::SidecarOpen,
        super::DebugWriterIoStage::Lock,
        super::DebugWriterIoStage::AppendOpen,
    ];
    for (index, stage) in stages.into_iter().enumerate() {
        assert!(writer.enqueue_io_fault(
            td.path().join(format!("{index}/events.jsonl")),
            b"{}\n".to_vec(),
            super::DebugWriterIoFault {
                stage,
                delay: if index == 0 {
                    Duration::from_millis(550)
                } else {
                    Duration::ZERO
                },
            },
        ));
    }
    wait_for_atomic(
        &writer.queue.completed_jobs,
        stages.len(),
        "early-failure timing records",
    );

    let traces = writer
        .queue
        .worker_traces
        .lock()
        .expect("worker trace mutex poisoned");
    assert_eq!(traces.len(), stages.len());
    for trace in traces.iter() {
        assert_eq!(
            trace.result,
            crate::debug_log::DebugLogCycleResult::AppendError
        );
        assert_eq!(trace.eof_us, 0);
        assert_eq!(trace.write_flush_us, 0);
        assert_eq!(trace.rollback_us, 0);
        assert_eq!(trace.line_bytes, 3);
        assert_eq!(trace.start_eof, None);
        assert_eq!(trace.end_eof, None);
    }
    assert!(traces[0].slow);
    assert!(traces[0].total_us >= 500_000);
}
