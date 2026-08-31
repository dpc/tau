use super::*;

/// Ensures a successful direct-child wait consumes the only cleanup obligation,
/// preventing Drop from attempting a second kill-and-wait.
#[cfg(target_os = "linux")]
#[test]
fn successful_wait_disarms_spawned_child_cleanup() {
    let child = Command::new("true")
        .spawn()
        .expect("successful-wait fixture should spawn");
    let mut guard = SpawnedChildGuard::new(child);

    let exit = guard
        .wait_for_exit()
        .expect("successful-wait fixture should exit");

    assert_eq!(exit.exit_code(), Some(0));
    assert!(guard.child.is_none());
}

/// Ensures a failed direct-child wait restores the cleanup obligation so Drop
/// retains the required best-effort kill-and-wait fallback.
#[cfg(target_os = "linux")]
#[test]
fn failed_wait_keeps_spawned_child_cleanup_armed() {
    let child = Command::new("true")
        .spawn()
        .expect("wait-error fixture should spawn");
    let pid = pid_to_rustix_pid(child.id()).expect("fixture PID should be valid");
    let mut guard = SpawnedChildGuard::new(child);

    process::waitpid(Some(pid), process::WaitOptions::empty())
        .expect("test should reap fixture child")
        .expect("fixture child should report an exit status");

    assert!(guard.wait_for_exit().is_err());
    assert!(guard.child.is_some());

    // The test reaped the child outside the guard. Do not let its ordinary
    // cleanup path signal a PID that the kernel could have reused.
    let _ = guard.child.take();
}
