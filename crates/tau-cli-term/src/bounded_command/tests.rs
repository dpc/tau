use std::sync::atomic::Ordering;
use std::{
    cell as path_std_cell, fs as path_std_fs, io as path_std_io, process as path_std_process,
    time as path_std_time,
};

use nix::unistd::Pid;

use super::*;

/// Foreground handoff binds ownership to Tau's actual process group, rejects an
/// initial mismatch before `tcsetpgrp`, and confirms only Tau after set
/// failure.
#[cfg(unix)]
#[test]
fn foreground_claim_injected_actual_tau_group_matrix() {
    let tau_pgid = Pid::from_raw(4100);
    let other_pgid = Pid::from_raw(4200);

    let set_attempts = path_std_cell::Cell::new(0);
    let claimed = claim_foreground_process_group_with(
        tau_pgid,
        || Err(path_nix_errno::Errno::ENOTTY),
        || {
            set_attempts.set(set_attempts.get() + 1);
            Ok(())
        },
    )
    .expect("no controlling terminal is a noninteractive no-handoff");
    assert!(!claimed);
    assert_eq!(
        set_attempts.get(),
        0,
        "noninteractive detection must precede handoff"
    );

    let set_attempts = path_std_cell::Cell::new(0);
    let error = claim_foreground_process_group_with(
        tau_pgid,
        || Err(path_nix_errno::Errno::EIO),
        || {
            set_attempts.set(set_attempts.get() + 1);
            Ok(())
        },
    )
    .expect_err("an unreadable initial foreground group must fail stop");
    assert!(error.is_foreground_ownership_unconfirmed());
    let diagnostic = error
        .foreground_restoration_diagnostic()
        .expect("initial query diagnostic");
    assert_eq!(diagnostic.class(), "initial-foreground-unconfirmed");
    assert_eq!(diagnostic.errno(), Some(path_nix_errno::Errno::EIO as i32));
    assert_eq!(
        set_attempts.get(),
        0,
        "failed initial query must precede handoff"
    );

    let set_attempts = path_std_cell::Cell::new(0);
    let error = claim_foreground_process_group_with(
        tau_pgid,
        || Ok(other_pgid),
        || {
            set_attempts.set(set_attempts.get() + 1);
            Ok(())
        },
    )
    .expect_err("initial non-Tau foreground group must fail stop");
    assert!(error.is_foreground_ownership_unconfirmed());
    let diagnostic = error
        .foreground_restoration_diagnostic()
        .expect("initial mismatch diagnostic");
    assert_eq!(diagnostic.class(), "initial-foreground-mismatch");
    assert_eq!(diagnostic.errno(), None);
    assert_eq!(set_attempts.get(), 0, "mismatch must precede handoff");

    let get_attempts = path_std_cell::Cell::new(0);
    let set_attempts = path_std_cell::Cell::new(0);
    let error = claim_foreground_process_group_with(
        tau_pgid,
        || {
            let attempt = get_attempts.get();
            get_attempts.set(attempt + 1);
            match attempt {
                0 => Err(path_nix_errno::Errno::EINTR),
                _ => Ok(tau_pgid),
            }
        },
        || {
            let attempt = set_attempts.get();
            set_attempts.set(attempt + 1);
            match attempt {
                0 => Err(path_nix_errno::Errno::EINTR),
                _ => Err(path_nix_errno::Errno::EPERM),
            }
        },
    )
    .expect_err("failed handoff remains an ordinary error only while Tau is foreground");
    assert!(!error.is_foreground_ownership_unconfirmed());
    assert_eq!(get_attempts.get(), 3);
    assert_eq!(set_attempts.get(), 2);

    let get_attempts = path_std_cell::Cell::new(0);
    let error = claim_foreground_process_group_with(
        tau_pgid,
        || {
            let attempt = get_attempts.get();
            get_attempts.set(attempt + 1);
            if attempt == 0 {
                Ok(tau_pgid)
            } else {
                Ok(other_pgid)
            }
        },
        || Err(path_nix_errno::Errno::EPERM),
    )
    .expect_err("changed non-Tau foreground group must fail stop");
    assert!(error.is_foreground_ownership_unconfirmed());
    let diagnostic = error
        .foreground_restoration_diagnostic()
        .expect("changed-foreground diagnostic");
    assert_eq!(diagnostic.class(), "foreground-handoff-unconfirmed");
    assert_eq!(
        diagnostic.errno(),
        Some(path_nix_errno::Errno::EPERM as i32)
    );
}

/// Restoration retries only interrupted syscalls, accepts an already-restored
/// foreground group, and retains the original non-EINTR `tcsetpgrp` errno.
#[cfg(unix)]
#[test]
fn foreground_restoration_injected_job_control_matrix() {
    let target = Pid::from_raw(4100);

    let set_attempts = path_std_cell::Cell::new(0);
    let get_attempts = path_std_cell::Cell::new(0);
    restore_foreground_process_group_with(
        target,
        || {
            let attempt = set_attempts.get();
            set_attempts.set(attempt + 1);
            if attempt < 2 {
                Err(path_nix_errno::Errno::EINTR)
            } else {
                Ok(())
            }
        },
        || {
            get_attempts.set(get_attempts.get() + 1);
            Ok(target)
        },
    )
    .expect("EINTR retries should reach successful restoration");
    assert_eq!(set_attempts.get(), 3);
    assert_eq!(get_attempts.get(), 0);

    let error = restore_foreground_process_group_with(
        target,
        || Err(path_nix_errno::Errno::ENOTTY),
        || Err(path_nix_errno::Errno::ENOTTY),
    )
    .expect_err("lost controlling terminal must retain fail-stop");
    assert_eq!(
        error.diagnostic(),
        ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(
            path_nix_errno::Errno::ENOTTY as i32
        )
    );

    let get_attempts = path_std_cell::Cell::new(0);
    restore_foreground_process_group_with(
        target,
        || Err(path_nix_errno::Errno::EPERM),
        || {
            let attempt = get_attempts.get();
            get_attempts.set(attempt + 1);
            if attempt == 0 {
                Err(path_nix_errno::Errno::EINTR)
            } else {
                Ok(target)
            }
        },
    )
    .expect("Tau already in the foreground confirms ownership");
    assert_eq!(get_attempts.get(), 2);

    let set_attempts = path_std_cell::Cell::new(0);
    let error = restore_foreground_process_group_with(
        target,
        || {
            set_attempts.set(set_attempts.get() + 1);
            Err(path_nix_errno::Errno::EAGAIN)
        },
        || Ok(Pid::from_raw(4200)),
    )
    .expect_err("a different foreground group must retain fail-stop");
    assert_eq!(set_attempts.get(), 1, "non-EINTR must not be retried");
    assert_eq!(
        error.diagnostic(),
        ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(
            path_nix_errno::Errno::EAGAIN as i32
        )
    );
}

/// A private PTY session exercises the same foreground process-group handoff
/// and restoration used by fzf inside job-control hosts such as tmux.
#[cfg(target_os = "linux")]
#[test]
fn pty_job_control_restores_external_foreground_owner() {
    const CHILD_ENV: &str = "TAU_PTY_FOREGROUND_RESTORE_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        pty_job_control_restore_child();
        return;
    }

    use std::os::unix::process::CommandExt as _;

    let pty = nix::pty::openpty(None, None).expect("open private pty");
    let slave = path_std_fs::File::from(pty.slave);
    let mut child =
        path_std_process::Command::new(std::env::current_exe().expect("current test executable"));
    let test_name = format!(
        "{}::pty_job_control_restores_external_foreground_owner",
        module_path!()
            .strip_prefix("tau_cli_term::")
            .unwrap_or(module_path!())
    );
    child
        .args(["--exact", &test_name, "--nocapture"])
        .env(CHILD_ENV, "1")
        .stdin(path_std_process::Stdio::from(
            slave.try_clone().expect("clone pty slave"),
        ))
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::piped());
    // SAFETY: the child-only hook invokes async-signal-safe session/tty
    // syscalls against its already-installed PTY stdin before exec.
    #[allow(unsafe_code)]
    unsafe {
        child.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(path_std_io::Error::last_os_error());
            }
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                return Err(path_std_io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn().expect("spawn PTY fixture");
    let mut child_stderr = child.stderr.take().expect("PTY fixture stderr");
    let deadline = path_std_time::Instant::now() + path_std_time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("poll PTY fixture") {
            let mut stderr = String::new();
            child_stderr
                .read_to_string(&mut stderr)
                .expect("read PTY fixture stderr");
            assert!(
                status.success(),
                "PTY fixture failed with {status}: {stderr}"
            );
            break;
        }
        if deadline <= path_std_time::Instant::now() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("PTY foreground restoration fixture timed out");
        }
        std::thread::sleep(path_std_time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn pty_job_control_restore_child() {
    let tau_pgid = nix::unistd::getpgrp();
    assert_eq!(
        nix::unistd::tcgetpgrp(std::io::stdin().as_fd()).expect("initial foreground group"),
        tau_pgid
    );

    let mut external = path_std_process::Command::new("sh");
    external
        .arg("-c")
        .arg("read -r _")
        .stdin(path_std_process::Stdio::piped());
    configure_process_group(&mut external).expect("configure external process group");
    let mut external = external.spawn().expect("spawn external foreground child");
    let release_external = external.stdin.take().expect("external child stdin");
    let external_pgid = Pid::from_raw(external.id() as i32);
    let mut handle =
        ProcessGroupHandle::claim_foreground(external.id()).expect("hand PTY to external child");
    assert_eq!(
        nix::unistd::tcgetpgrp(std::io::stdin().as_fd()).expect("external foreground group"),
        external_pgid
    );
    drop(release_external);
    let _ = external.wait().expect("wait external child");
    handle
        .restore_foreground()
        .expect("restore Tau foreground group");
    assert_eq!(
        nix::unistd::tcgetpgrp(std::io::stdin().as_fd()).expect("restored foreground group"),
        tau_pgid
    );

    restore_foreground_process_group_with(
        tau_pgid,
        || Err(path_nix_errno::Errno::EPERM),
        || nix::unistd::tcgetpgrp(std::io::stdin().as_fd()),
    )
    .expect("already-restored Tau foreground group is confirmed");
}

/// Checked restoration preserves successful, nonzero, and waiter-error child
/// outcomes while classifying persistent restoration failure as fail-stop.
#[cfg(unix)]
#[test]
fn restoration_failure_matrix_preserves_primary_outcomes() {
    use std::os::unix::process::ExitStatusExt as _;

    let rows = [
        (
            Ok(BoundedCommandStatus {
                status: path_std_process::ExitStatus::from_raw(0),
            }),
            "exit status: 0",
        ),
        (
            Ok(BoundedCommandStatus {
                status: path_std_process::ExitStatus::from_raw(7 << 8),
            }),
            "exit status: 7",
        ),
        (
            Err("could not wait for command: injected waiter failure".to_owned()),
            "injected waiter failure",
        ),
    ];

    for (primary, expected_primary) in rows {
        let restore_attempts = path_std_cell::Cell::new(0);
        let error = settle_after_child_with_restore(
            primary,
            |output| format!("command exited with {}", output.status),
            || {
                restore_attempts.set(restore_attempts.get() + 1);
                Err(ForegroundRestorationError::tcsetpgrp_unconfirmed(
                    path_nix_errno::Errno::EIO,
                ))
            },
        )
        .expect_err("persistent restoration failure must fail stop");

        let BoundedCommandError::ForegroundOwnershipUnconfirmed {
            primary,
            restoration,
        } = error
        else {
            panic!("wrong failure classification");
        };
        assert!(
            primary.contains(expected_primary),
            "primary was {primary:?}"
        );
        assert_eq!(
            restoration.diagnostic(),
            ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(
                path_nix_errno::Errno::EIO as i32
            )
        );
        assert_eq!(restore_attempts.get(), 1);
    }
}

/// Timeout cleanup reaches bounded non-runnable child-group termination and
/// retains both timeout and persistent foreground-restoration failures.
#[cfg(target_os = "linux")]
#[test]
fn inherited_timeout_restoration_failure_kills_child_group() {
    let _guard = FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground restore test lock");
    let dir = tempfile::tempdir().expect("pid directory");
    let pid_path = dir.path().join("descendant.pid");
    let pending_pid_path = dir.path().join("descendant.pid.pending");
    let script = format!(
        "sleep 30 & printf '%s\n' $! > {} && mv {} {}; wait",
        pending_pid_path.display(),
        pending_pid_path.display(),
        pid_path.display()
    );
    let mut command = path_std_process::Command::new("sh");
    command
        .args(["-c", &script])
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null());
    FOREGROUND_RESTORE_ATTEMPTS.store(0, Ordering::SeqCst);
    FAIL_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);
    let cleanup_started = path_std_cell::Cell::new(None);

    let error = run_with_inherited_stdio_after_spawn(
        &mut command,
        path_std_time::Duration::from_millis(100),
        ProcessOwnership::ForegroundProcessGroup,
        || {
            let deadline = path_std_time::Instant::now() + path_std_time::Duration::from_secs(2);
            while !pid_path.exists() {
                if deadline <= path_std_time::Instant::now() {
                    return Err("descendant pid was not published".to_owned());
                }
                std::thread::yield_now();
            }
            cleanup_started.set(Some(path_std_time::Instant::now()));
            Ok(())
        },
    )
    .expect_err("timeout plus restoration failure must fail stop");
    FAIL_FOREGROUND_RESTORE.store(false, Ordering::SeqCst);

    let BoundedCommandError::ForegroundOwnershipUnconfirmed {
        primary,
        restoration,
    } = error
    else {
        panic!("wrong failure classification");
    };
    assert!(primary.contains("timeout"), "primary was {primary:?}");
    assert_eq!(
        restoration.diagnostic(),
        ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(path_nix_errno::Errno::EIO as i32)
    );
    assert_eq!(
        FOREGROUND_RESTORE_ATTEMPTS.load(Ordering::SeqCst),
        2,
        "checked restore plus Drop fallback"
    );
    assert!(
        cleanup_started
            .get()
            .expect("cleanup phase started")
            .elapsed()
            < path_std_time::Duration::from_secs(2)
    );

    let pid = std::fs::read_to_string(&pid_path)
        .expect("descendant pid")
        .trim()
        .parse::<i32>()
        .expect("numeric descendant pid");
    let cleanup_deadline = path_std_time::Instant::now() + path_std_time::Duration::from_secs(1);
    let last_state = loop {
        let process_state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").map(|(_, tail)| tail.to_owned()))
            .and_then(|tail| tail.chars().next());
        if process_state.is_none_or(|state| state == 'Z') {
            break process_state;
        }
        if cleanup_deadline <= path_std_time::Instant::now() {
            break process_state;
        }
        std::thread::yield_now();
    };
    assert!(
        last_state.is_none_or(|state| state == 'Z'),
        "descendant process {pid} remains runnable in state {last_state:?}"
    );
}

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
    FAIL_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);

    let error = handle
        .restore_foreground()
        .expect_err("injected restore failure");
    FAIL_FOREGROUND_RESTORE.store(false, Ordering::SeqCst);

    assert_eq!(
        error.diagnostic(),
        ForegroundRestorationDiagnostic::tcsetpgrp_unconfirmed(path_nix_errno::Errno::EIO as i32)
    );
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
    let mut command = path_std_process::Command::new("sh");
    command
        .args(["-c", "printf done"])
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());
    FAIL_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);

    let error = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_secs(2),
        ProcessOwnership::ForegroundProcessGroup,
    )
    .expect_err("restore failure must replace otherwise successful output");
    FAIL_FOREGROUND_RESTORE.store(false, Ordering::SeqCst);

    assert!(error.to_string().contains("tcsetpgrp-unconfirmed"));
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
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("yes overflow")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    let start = path_std_time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("overflow should fail");

    assert!(err.to_string().contains("stdout exceeded"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Covers children that write substantial stdout before reading stdin; stdout
/// draining must already be active while Tau writes prompt-history rows.
#[test]
fn bounded_command_drains_stdout_while_writing_stdin() {
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%65536s' x; bytes=$(wc -c); printf '\\n%s' \"$bytes\"")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());
    let stdin = vec![b'y'; 65536];

    let output = run_with_bounded_stdout(
        &mut command,
        Some(&stdin),
        200_000,
        path_std_time::Duration::from_secs(5),
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
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    let start = path_std_time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("inherited stdout holder should fail promptly");

    assert!(err.to_string().contains("stdout pipe did not close"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(path_std_time::Duration::from_millis(200));
    let alive = path_std_process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "stdout holder {pid} should have been killed");
}

/// Ensures a hung child that never writes enough output to overflow is still
/// bounded by the elapsed command timeout.
#[test]
fn bounded_command_times_out_quiet_hung_child() {
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    let start = path_std_time::Instant::now();
    let err = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("quiet hung child should time out");

    assert!(err.to_string().contains("timeout"));
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
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    let err = run_with_bounded_stdout_after_spawn(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
        || {
            let deadline = path_std_time::Instant::now() + path_std_time::Duration::from_secs(2);
            while !pid_path.exists() {
                if deadline <= path_std_time::Instant::now() {
                    return Err("descendant PID was not published after command spawn".to_owned());
                }
                std::thread::yield_now();
            }
            Ok(())
        },
    )
    .expect_err("process group should time out");
    assert!(err.to_string().contains("timeout"));

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    std::thread::sleep(path_std_time::Duration::from_millis(200));
    let alive = path_std_process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null())
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
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::null());

    FAIL_NEXT_FOREGROUND_CLAIM.store(true, Ordering::SeqCst);
    let error = run_with_bounded_stdout(
        &mut command,
        None,
        1024,
        path_std_time::Duration::from_secs(5),
        ProcessOwnership::ForegroundProcessGroup,
    )
    .expect_err("foreground handoff should fail");
    assert!(error.to_string().contains("could not hand terminal"));

    let pid = LAST_FAILED_FOREGROUND_CHILD_ID.load(Ordering::SeqCst);
    assert_ne!(pid, 0, "test seam did not record spawned child pid");
    std::thread::sleep(path_std_time::Duration::from_millis(200));
    let alive = path_std_process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "spawned child {pid} should have been killed");
}

/// Covers terminal-style prompt edit actions that inherit stdio instead of
/// capturing stdout; they must still be bounded by timeout and process-group
/// cleanup so a stuck editor cannot leave Tau paused indefinitely.
#[test]
fn inherited_stdio_command_times_out_quiet_hung_child() {
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 5")
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null());

    let start = path_std_time::Instant::now();
    let err = run_with_inherited_stdio(
        &mut command,
        path_std_time::Duration::from_millis(100),
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("quiet hung child should time out");

    assert!(err.to_string().contains("timeout"));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

/// Covers the successful inherited-stdio path so child-waiter event handling
/// returns the direct child's status without requiring captured pipe workers.
#[test]
fn inherited_stdio_command_returns_child_status() {
    let mut command = path_std_process::Command::new("sh");
    command
        .arg("-c")
        .arg("exit 7")
        .stdin(path_std_process::Stdio::null())
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null());

    let output = run_with_inherited_stdio(
        &mut command,
        path_std_time::Duration::from_secs(5),
        ProcessOwnership::ProcessGroup,
    )
    .expect("short inherited-stdio command should finish");

    assert_eq!(output.status.code(), Some(7));
}
