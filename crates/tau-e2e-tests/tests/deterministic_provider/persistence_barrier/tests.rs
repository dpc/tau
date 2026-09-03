use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use nix::poll::{PollFd, PollFlags, poll};

use super::super::daemon_support::open_pidfd;
use super::*;

/// Creates one private barrier and returns its client socket path.
fn test_barrier() -> (tempfile::TempDir, PathBuf, PersistenceBarrier) {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let path = tempdir.path().join("barrier.sock");
    let barrier = PersistenceBarrier::bind(&path, OutputLengthCrashCut::PlannedResponse)
        .expect("bind barrier");
    (tempdir, path, barrier)
}

/// Creates a process-readiness descriptor that remains quiet for the test.
fn quiet_process_fd() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("process readiness pair")
}

/// Sends a complete fixed protocol transcript to one bound observer.
fn send_protocol(path: &Path, pid: u32, outcome: &str) {
    let mut stream = UnixStream::connect(path).expect("connect barrier");
    writeln!(
        stream,
        "tau-persistence-barrier-v1 hook cut=planned-response pid={pid} durability_timeout_ms=5000"
    )
    .expect("write hook");
    writeln!(
        stream,
        "tau-persistence-barrier-v1 outcome={outcome} elapsed_ms=17"
    )
    .expect("write outcome");
}

/// Spawns a real child that writes its own PID and optional complete outcome,
/// then exits immediately with status 9.
fn spawn_exiting_protocol_child(path: &Path, outcome: Option<&str>) -> (Child, OwnedFd) {
    let stream = UnixStream::connect(path).expect("connect child protocol stream");
    let outcome = outcome
        .map(|outcome| {
            format!("printf 'tau-persistence-barrier-v1 outcome={outcome} elapsed_ms=17\\n';")
        })
        .unwrap_or_default();
    let stdout: OwnedFd = stream.into();
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf 'tau-persistence-barrier-v1 hook cut=planned-response pid=%s durability_timeout_ms=5000\\n' \"$$\"; {outcome} exit 9"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn exiting protocol child");
    let pidfd = open_pidfd(child.id()).expect("open protocol child pidfd");
    (child, pidfd)
}

/// Boundedly establishes child exit and a closed producer transcript before
/// observer arbitration begins.
fn require_protocol_child_exit(child: &mut Child, pidfd: &OwnedFd) {
    let flags = PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR;
    let mut descriptors = [PollFd::new(pidfd.as_fd(), flags)];
    assert_ne!(
        poll(&mut descriptors, 1_000_u16).expect("poll protocol child pidfd"),
        0,
        "protocol child exceeded exit deadline"
    );
    assert!(
        descriptors[0]
            .revents()
            .is_some_and(|events| events.intersects(flags)),
        "protocol child pidfd woke without exit readiness"
    );
    assert_eq!(
        child
            .try_wait()
            .expect("reap protocol child")
            .expect("pidfd-ready child has terminal status")
            .code(),
        Some(9)
    );
}

/// Requires stable diagnostic identity facts on one classified failure.
fn assert_failure_facts(error: &PersistenceBarrierFailure, outcome: &str, pid: u32) {
    let diagnostic = error.to_string();
    assert!(diagnostic.contains(outcome), "{diagnostic}");
    assert!(diagnostic.contains("cut=planned-response"), "{diagnostic}");
    assert!(diagnostic.contains(&format!("pid={pid}")), "{diagnostic}");
    assert!(diagnostic.contains("hook_elapsed_ms="), "{diagnostic}");
}

/// Accepts a durable producer result as the sole successful outcome.
#[test]
fn persistence_barrier_accepts_durable_outcome() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || send_protocol(&path, 41, "durable"));
    barrier
        .wait_with_process(
            41,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect("durable outcome");
    sender.join().expect("sender");
}

/// Preserves a producer durability timeout instead of replacing it with an
/// observer timeout.
#[test]
fn persistence_barrier_reports_producer_durability_timeout() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || send_protocol(&path, 42, "durability-timeout"));
    let error = barrier
        .wait_with_process(
            42,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("producer timeout");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ProducerDurabilityTimeout
    );
    assert_failure_facts(&error, "producer outcome=durability-timeout", 42);
    assert!(error.to_string().contains("producer_elapsed_ms=17"));
    assert!(error.to_string().contains("durability_timeout_ms=5000"));
    sender.join().expect("sender");
}

/// Preserves an unavailable or failed durability worker as producer failure.
#[test]
fn persistence_barrier_reports_producer_durability_failure() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || send_protocol(&path, 43, "durability-failed"));
    let error = barrier
        .wait_with_process(
            43,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("producer failure");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ProducerDurabilityFailed
    );
    assert_failure_facts(&error, "producer outcome=durability-failed", 43);
    assert!(error.to_string().contains("producer_elapsed_ms=17"));
    assert!(error.to_string().contains("durability_timeout_ms=5000"));
    assert!(error.to_string().contains("protocol_timeout_ms=1000"));
    sender.join().expect("sender");
}

/// Classifies a bounded absence of any producer connection as hook-not-reached.
#[test]
fn persistence_barrier_reports_hook_not_reached() {
    let (_tempdir, _path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let error = barrier
        .wait_with_process(
            44,
            process.as_fd(),
            Duration::from_millis(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("missing hook");
    assert_eq!(error.kind, PersistenceBarrierFailureKind::HookNotReached);
    assert_failure_facts(&error, "producer outcome=hook-not-reached", 44);
    assert!(error.to_string().contains("hook_timeout_ms=1"));
}

/// Reports an actual child exit after a valid hook separately from transport
/// failure, even when stream closure is concurrently ready.
#[test]
fn persistence_barrier_reports_premature_daemon_exit_after_hook() {
    let (_tempdir, path, barrier) = test_barrier();
    let (mut child, pidfd) = spawn_exiting_protocol_child(&path, None);
    let pid = child.id();
    require_protocol_child_exit(&mut child, &pidfd);
    let error = barrier
        .wait_with_process(
            pid,
            pidfd.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || {
                child
                    .try_wait()
                    .map(|status| {
                        status.map(|status| format!("daemon status={status}; daemon_stderr=empty"))
                    })
                    .map_err(|error| error.to_string())
            },
        )
        .expect_err("premature exit");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::PrematureDaemonExit,
        "{error}"
    );
    assert_failure_facts(&error, "producer outcome=premature-daemon-exit", pid);
    assert!(error.to_string().contains("phase=producer-outcome"));
    assert!(error.to_string().contains("daemon status=exit status: 9"));
}

/// Preserves a complete producer failure outcome even when the same real child
/// exits immediately after writing it.
#[test]
fn persistence_barrier_drains_complete_failure_before_classifying_exit() {
    let (_tempdir, path, barrier) = test_barrier();
    let (mut child, pidfd) = spawn_exiting_protocol_child(&path, Some("durability-timeout"));
    let pid = child.id();
    require_protocol_child_exit(&mut child, &pidfd);
    let error = barrier
        .wait_with_process(
            pid,
            pidfd.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || {
                child
                    .try_wait()
                    .map(|status| {
                        status.map(|status| format!("daemon status={status}; daemon_stderr=empty"))
                    })
                    .map_err(|error| error.to_string())
            },
        )
        .expect_err("producer timeout survives exit");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ProducerDurabilityTimeout,
        "{error}"
    );
    assert_failure_facts(&error, "producer outcome=durability-timeout", pid);
}

/// Reports a pidfd-ready exit-probe failure as observer protocol failure.
#[test]
fn persistence_barrier_reports_exit_probe_failure() {
    let (_tempdir, _path, barrier) = test_barrier();
    let (process, mut signal) = quiet_process_fd();
    signal.write_all(b"x").expect("signal process readiness");
    let error = barrier
        .wait_with_process(
            45,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Err("injected probe failure".to_owned()),
        )
        .expect_err("probe failure");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 45);
    assert!(error.to_string().contains("phase=before-hook"));
    assert!(error.to_string().contains("injected probe failure"));
}

/// Distinguishes a malformed hook identity from producer-side failure.
#[test]
fn persistence_barrier_reports_invalid_hook_protocol() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(path).expect("connect barrier");
        writeln!(stream, "not-the-protocol").expect("write malformed hook");
    });
    let error = barrier
        .wait_with_process(
            46,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("protocol failure");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 46);
    assert!(error.to_string().contains("phase=hook-identity"));
    assert!(error.to_string().contains("protocol_timeout_ms=1000"));
    sender.join().expect("sender");
}

/// Rejects a syntactically valid record carrying an unknown outcome name.
#[test]
fn persistence_barrier_reports_unknown_outcome() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || send_protocol(&path, 47, "mystery"));
    let error = barrier
        .wait_with_process(
            47,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("unknown outcome");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 47);
    assert!(
        error
            .to_string()
            .contains("invalid outcome name=\"mystery\"")
    );
    sender.join().expect("sender");
}

/// Rejects a valid hook followed by a malformed producer elapsed-time field.
#[test]
fn persistence_barrier_reports_malformed_outcome_elapsed_time() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(path).expect("connect barrier");
        writeln!(
            stream,
            "tau-persistence-barrier-v1 hook cut=planned-response pid=50 durability_timeout_ms=5000"
        )
        .expect("write hook");
        writeln!(
            stream,
            "tau-persistence-barrier-v1 outcome=durability-timeout elapsed_ms=invalid"
        )
        .expect("write malformed outcome");
    });
    let error = barrier
        .wait_with_process(
            50,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("malformed elapsed time");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 50);
    assert!(error.to_string().contains("phase=producer-outcome"));
    assert!(error.to_string().contains("elapsed_ms=invalid"));
    sender.join().expect("sender");
}

/// Rejects one protocol record before reading beyond its fixed byte cap.
#[test]
fn persistence_barrier_rejects_oversized_record() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let sender = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(path).expect("connect barrier");
        stream
            .write_all(&vec![b'x'; protocol_reader::MAX_PROTOCOL_LINE_BYTES + 1])
            .expect("write oversized record");
    });
    let error = barrier
        .wait_with_process(
            48,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            || Ok(None),
        )
        .expect_err("oversized record");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 48);
    assert!(error.to_string().contains("exceeded 512 bytes"));
    sender.join().expect("sender");
}

/// Applies one absolute deadline to a held-open partial record instead of
/// renewing the timeout after each read.
#[test]
fn persistence_barrier_bounds_held_open_partial_record() {
    let (_tempdir, path, barrier) = test_barrier();
    let (process, _hold) = quiet_process_fd();
    let (release_send, release_receive) = mpsc::sync_channel(0);
    let sender = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(path).expect("connect barrier");
        stream.write_all(b"partial").expect("write partial record");
        release_receive.recv().expect("release held stream");
    });
    let error = barrier
        .wait_with_process(
            49,
            process.as_fd(),
            Duration::from_secs(1),
            Duration::from_millis(5),
            || Ok(None),
        )
        .expect_err("partial deadline");
    release_send.send(()).expect("release sender");
    sender.join().expect("sender");
    assert_eq!(
        error.kind,
        PersistenceBarrierFailureKind::ObserverProtocolFailure
    );
    assert_failure_facts(&error, "producer outcome=unknown", 49);
    assert!(error.to_string().contains("absolute deadline expired"));
    assert!(error.to_string().contains("protocol_timeout_ms=5"));
}
