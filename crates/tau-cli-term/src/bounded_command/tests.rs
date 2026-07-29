use std::sync::atomic::Ordering;

use super::*;

/// Foreground restoration is a visible error, while the still-armed handle
/// retains its Drop fallback for a second best-effort attempt.
#[cfg(unix)]
#[test]
fn foreground_restore_failure_is_reported() {
    let _guard = FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground restore test lock");
    let mut handle = ProcessGroupHandle {
        child_pgid: None,
        parent_pgid: Some(nix::unistd::getpgrp()),
    };
    FAIL_NEXT_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);

    let error = handle
        .restore_foreground()
        .expect_err("injected restore failure");

    assert!(error.contains("restore Tau terminal foreground"));
    assert!(handle.parent_pgid.is_some(), "Drop fallback remains armed");
}

/// The bounded runner propagates restoration failure after collecting a child,
/// rather than returning output while Tau may still be backgrounded.
#[cfg(unix)]
#[test]
fn bounded_command_propagates_foreground_restore_failure() {
    let _guard = FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground restore test lock");
    let mut command = std::process::Command::new("sh");
    command
        .args(["-c", "printf done"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    FAIL_NEXT_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);

    let error = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        std::time::Duration::from_secs(2),
        ProcessOwnership::ForegroundProcessGroup,
    )
    .expect_err("restore failure must replace otherwise successful output");

    assert!(error.contains("restore Tau terminal foreground"));
}

/// Prevents external prompt/completion commands from allocating unbounded
/// memory when a misconfigured command writes a very large stdout stream.
#[test]
fn bounded_stdout_reader_reports_overflow_without_storing_tail() {
    let input = vec![b'x'; crate::PROMPT_COMMAND_OUTPUT_LIMIT_BYTES + 17];

    let read = read_to_limit(input.as_slice(), crate::PROMPT_COMMAND_OUTPUT_LIMIT_BYTES)
        .expect("in-memory read should succeed");

    assert!(read.overflowed);
    assert_eq!(read.bytes.len(), crate::PROMPT_COMMAND_OUTPUT_LIMIT_BYTES);
}

/// Ensures an over-limit child is killed and reported promptly instead of
/// leaving the prompt paused while Tau drains an endless stdout stream.
#[test]
fn bounded_command_kills_child_on_stdout_overflow() {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("yes overflow")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let start = std::time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        std::time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("overflow should fail");

    assert!(err.contains("stdout exceeded"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Covers children that write substantial stdout before reading stdin; stdout
/// draining must already be active while Tau writes prompt-history rows.
#[test]
fn bounded_command_drains_stdout_while_writing_stdin() {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%65536s' x; bytes=$(wc -c); printf '\\n%s' \"$bytes\"")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let stdin = vec![b'y'; 65536];

    let output = run_with_bounded_stdout(
        &mut command,
        Some(&stdin),
        200_000,
        std::time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect("interleaved stdin/stdout command should finish");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("test output utf-8");
    assert!(stdout.ends_with("\n65536"), "stdout was {stdout:?}");
}

/// Ensures a direct child that exits after spawning a background process with
/// inherited stdout does not leave Tau waiting forever for pipe EOF.
#[test]
fn bounded_command_errors_when_stdout_holder_survives_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_path = dir.path().join("holder.pid");
    let script = format!("sleep 3 & echo $! > {}; printf done", pid_path.display());
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let start = std::time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        std::time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("inherited stdout holder should fail promptly");

    assert!(err.contains("stdout pipe did not close"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "stdout holder {pid} should have been killed");
}

/// Ensures a hung child that never writes enough output to overflow is still
/// bounded by the elapsed command timeout.
#[test]
fn bounded_command_times_out_quiet_hung_child() {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let start = std::time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        std::time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("quiet hung child should time out");

    assert!(err.contains("timeout"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Ensures process-group-owned prompt actions terminate descendants before the
/// helper returns on timeout, preventing orphaned TUI/editor children from
/// retaining terminal ownership after Tau resumes raw mode.
#[cfg(unix)]
#[test]
fn process_group_timeout_kills_descendant() {
    let _foreground_claim_guard = FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground claim test lock");
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_path = dir.path().join("child.pid");
    let pending_pid_path = dir.path().join("child.pid.pending");
    let script = format!(
        "sleep 5 & printf '%s\n' $! > {} && mv {} {}; sleep 5",
        pending_pid_path.display(),
        pending_pid_path.display(),
        pid_path.display()
    );
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let err = run_with_bounded_stdout_after_spawn(
        &mut command,
        None,
        1024,
        std::time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
        || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !pid_path.exists() {
                if deadline <= std::time::Instant::now() {
                    return Err("descendant PID was not published after command spawn".to_owned());
                }
                std::thread::yield_now();
            }
            Ok(())
        },
    )
    .expect_err("process group should time out");
    assert!(err.contains("timeout"));

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "descendant process {pid} should have been killed");
}

/// Ensures a post-spawn foreground handoff failure kills and reaps the already
/// spawned prompt-action process group before returning the setup error.
#[cfg(unix)]
#[test]
fn process_group_setup_failure_kills_spawned_child() {
    let _foreground_claim_guard = FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground claim test lock");
    LAST_FAILED_FOREGROUND_CHILD_ID.store(0, Ordering::SeqCst);
    FAIL_FOREGROUND_CLAIM_FOR_CHILD_ID.store(0, Ordering::SeqCst);
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    FAIL_NEXT_FOREGROUND_CLAIM.store(true, Ordering::SeqCst);
    let error = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        std::time::Duration::from_secs(5),
        ProcessOwnership::ForegroundProcessGroup,
    )
    .expect_err("foreground handoff should fail");
    assert!(error.contains("could not hand terminal"));

    let pid = LAST_FAILED_FOREGROUND_CHILD_ID.load(Ordering::SeqCst);
    assert_ne!(pid, 0, "test seam did not record spawned child pid");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "spawned child {pid} should have been killed");
}

/// Covers terminal-style prompt edit actions that inherit stdio instead of
/// capturing stdout; they must still be bounded by timeout and process-group
/// cleanup so a stuck editor cannot leave Tau paused indefinitely.
#[test]
fn inherited_stdio_command_times_out_quiet_hung_child() {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = std::time::Instant::now();
    let err = run_with_inherited_stdio(
        &mut command,
        std::time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("quiet hung child should time out");

    assert!(err.contains("timeout"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Covers the successful inherited-stdio path so child-waiter event handling
/// returns the direct child's status without requiring captured pipe workers.
#[test]
fn inherited_stdio_command_returns_child_status() {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("exit 7")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let output = run_with_inherited_stdio(
        &mut command,
        std::time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect("short inherited-stdio command should finish");

    assert_eq!(output.status.code(), Some(7));
}
