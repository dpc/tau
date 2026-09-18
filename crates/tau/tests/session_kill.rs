//! Hermetic lifecycle coverage for `tau session kill`.

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::stat::Mode;
use nix::unistd::{Pid, mkfifo};

mod support;

const RELEASE_RETRY_DELAY: Duration = Duration::from_millis(10);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const FAILURE_CLEANUP_GRACE: Duration = Duration::from_secs(3);
const FORCED_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the bundled Tau binary under Cargo's integration-test contract.
fn tau_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_tau")
        .map(PathBuf::from)
        .expect("CARGO_BIN_EXE_tau")
}

/// Builds one isolated Tau command with no bundled extensions enabled.
fn command(root: &Path) -> Command {
    let mut command = base_command(root);
    command.arg("--disable-extensions-all");
    command
}

/// Builds one isolated Tau command while retaining file-based extension config.
fn base_command(root: &Path) -> Command {
    support::isolated_tau_command(tau_bin(), root)
}

/// Starts one durable headless session under the isolated test root.
fn spawn_session(root: &Path, session_id: &str) -> Child {
    command(root)
        .args(["serve", "--session", session_id, "--create"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn session server")
}

/// Waits until session discovery reports one exact identifier while proving
/// that its foreground server remains alive during startup.
fn wait_for_session(root: &Path, expected: &str, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert_eq!(
            child.try_wait().expect("query starting server"),
            None,
            "session server exited before becoming discoverable"
        );
        let output = command(root)
            .args(["session", "list"])
            .output()
            .expect("list sessions");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success() && stdout.lines().any(|line| line == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session {expected:?} did not become discoverable; stdout={stdout:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits boundedly for a server's graceful successful exit.
fn wait_for_success(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("query server exit") {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Opens and writes the canary release token without ever blocking past one
/// absolute deadline.
fn release_fifo(
    path: &Path,
    deadline: Instant,
    mut check_children: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    loop {
        check_release_deadline(deadline, "opening release FIFO")?;
        match OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut fifo) => return write_release(&mut fifo, deadline, check_children),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(nix::libc::ENXIO | nix::libc::EINTR)
                ) =>
            {
                check_children()?;
                sleep_until_retry(deadline);
            }
            Err(error) => {
                return Err(format!(
                    "open release FIFO {} failed: {error}",
                    path.display()
                ));
            }
        }
    }
}

/// Writes the complete release token through an already-open nonblocking FIFO.
fn write_release(
    fifo: &mut File,
    deadline: Instant,
    mut check_children: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let release = b"release\n";
    let mut written = 0;
    while written < release.len() {
        check_release_deadline(deadline, "writing release FIFO")?;
        match fifo.write(&release[written..]) {
            Ok(0) => return Err("release FIFO accepted zero bytes".to_owned()),
            Ok(count) => written += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                check_children()?;
                sleep_until_retry(deadline);
            }
            Err(error) => return Err(format!("write release FIFO failed: {error}")),
        }
    }
    Ok(())
}

/// Rejects release work once its shared absolute deadline has expired.
fn check_release_deadline(deadline: Instant, action: &str) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(format!("deadline expired while {action}"));
    }
    Ok(())
}

/// Delays one retry without sleeping beyond the shared release deadline.
fn sleep_until_retry(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(RELEASE_RETRY_DELAY.min(remaining));
}

/// Rejects release retries after either owned process exits prematurely.
fn check_shutdown_children(server: &mut Child, kill: &mut Child) -> Result<(), String> {
    if let Some(status) = kill
        .try_wait()
        .map_err(|error| format!("query kill command during release: {error}"))?
    {
        return Err(format!(
            "kill command exited before canary release: {status}"
        ));
    }
    if let Some(status) = server
        .try_wait()
        .map_err(|error| format!("query server during release: {error}"))?
    {
        return Err(format!("server exited before canary release: {status}"));
    }
    Ok(())
}

/// Gives canonical shutdown a bounded opportunity, then kills the server's
/// process group before terminating and reaping the two owned children.
fn cleanup_failed_shutdown(
    server: &mut Child,
    kill: &mut Child,
    server_pgid: u32,
    grace: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + grace;
    loop {
        let server_done = server
            .try_wait()
            .map_err(|error| format!("query server during failure cleanup: {error}"))?
            .is_some();
        let kill_done = kill
            .try_wait()
            .map_err(|error| format!("query kill command during failure cleanup: {error}"))?
            .is_some();
        if server_done && kill_done {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(RELEASE_RETRY_DELAY);
    }

    let pgid = Pid::from_raw(
        i32::try_from(server_pgid).map_err(|_| "server PGID exceeds i32".to_owned())?,
    );
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => return Err(format!("kill server process group {server_pgid}: {error}")),
    }
    let _ = server.kill();
    let _ = kill.kill();

    let reap_deadline = Instant::now() + FORCED_REAP_TIMEOUT;
    reap_owned_child(server, "server", reap_deadline)?;
    reap_owned_child(kill, "kill command", reap_deadline)
}

/// Reaps one owned child while retaining a finite forced-cleanup bound.
fn reap_owned_child(child: &mut Child, name: &str, deadline: Instant) -> Result<(), String> {
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("reap {name}: {error}"))?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("{name} did not exit after forced cleanup"));
        }
        std::thread::sleep(RELEASE_RETRY_DELAY);
    }
}

/// Exact-session kill uses canonical shutdown, leaves another daemon alone,
/// rejects absence, retires runtime discovery, and preserves durable history.
#[test]
fn exact_session_kill_preserves_history_and_other_sessions() {
    let root = support::bounded_runtime_tempdir();
    let mut target = spawn_session(root.path(), "kill-target");
    wait_for_session(root.path(), "kill-target", &mut target);
    let mut other = spawn_session(root.path(), "kill-other");
    wait_for_session(root.path(), "kill-other", &mut other);

    let missing = command(root.path())
        .args(["session", "kill", "does-not-exist"])
        .output()
        .expect("kill missing session");
    assert!(!missing.status.success(), "missing session kill succeeded");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("is not currently running"),
        "unexpected missing-session error: {:?}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let killed = command(root.path())
        .args(["session", "kill", "kill-target"])
        .output()
        .expect("kill exact session");
    assert!(
        killed.status.success(),
        "session kill failed: {:?}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&killed.stdout),
        "Session `kill-target` terminated\n"
    );
    wait_for_success(&mut target);

    let listed = command(root.path())
        .args(["session", "list"])
        .output()
        .expect("list remaining session");
    assert!(listed.status.success(), "remaining session list failed");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(!listed.lines().any(|line| line == "kill-target"));
    assert!(listed.lines().any(|line| line == "kill-other"));
    assert_eq!(other.try_wait().expect("query other server"), None);

    let shown = command(root.path())
        .args(["session", "show", "--session-id", "kill-target"])
        .output()
        .expect("show killed session history");
    assert!(
        shown.status.success(),
        "killed session history unavailable: {:?}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(
        root.path().join(".state/tau/sessions/kill-target").is_dir(),
        "killed session storage was removed"
    );

    let cleanup = command(root.path())
        .args(["session", "kill", "kill-other"])
        .output()
        .expect("kill remaining session");
    assert!(
        cleanup.status.success(),
        "remaining session cleanup failed: {:?}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
    wait_for_success(&mut other);
}

/// Runtime socket policy remains authoritative: an inaccessible exact session
/// fails cleanly instead of falling back to signaling or deleting state.
#[test]
fn inaccessible_session_socket_is_not_bypassed() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    let root = support::bounded_runtime_tempdir();
    let mut server = spawn_session(root.path(), "kill-inaccessible");
    wait_for_session(root.path(), "kill-inaccessible", &mut server);
    let sockets_dir = support::isolated_runtime_dir(root.path()).join("tau/harnesses/sockets");
    let socket = std::fs::read_dir(&sockets_dir)
        .expect("read sockets directory")
        .map(|entry| entry.expect("socket entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sock")
        })
        .expect("session socket");
    let original = std::fs::metadata(&socket)
        .expect("socket metadata")
        .permissions();
    std::fs::set_permissions(&socket, Permissions::from_mode(0o000))
        .expect("make socket inaccessible");

    let denied = command(root.path())
        .args(["session", "kill", "kill-inaccessible"])
        .output()
        .expect("attempt inaccessible session kill");
    std::fs::set_permissions(&socket, original).expect("restore socket permissions");

    assert!(
        !denied.status.success(),
        "inaccessible session kill succeeded"
    );
    assert!(
        server
            .try_wait()
            .expect("query inaccessible server")
            .is_none(),
        "failed kill stopped the server"
    );
    let cleanup = command(root.path())
        .args(["session", "kill", "kill-inaccessible"])
        .output()
        .expect("clean up inaccessible session");
    assert!(
        cleanup.status.success(),
        "cleanup kill failed: {:?}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
    wait_for_success(&mut server);
}

/// The command must not turn socket EOF or a successful request write into a
/// termination claim while the exact admitted daemon remains alive.
#[test]
fn session_kill_waits_for_the_exact_daemon_process_exit() {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    let root = support::bounded_runtime_tempdir();
    let _ = base_command(root.path());
    let script = root.path().join("shutdown-canary");
    let stopped = root.path().join("extension-stopped");
    let release = root.path().join("release-extension-wrapper");
    mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");
    std::fs::write(
        &script,
        "#!/bin/sh\n\"$1\" component ext-std-notifications\nstatus=$?\nprintf stopped > \"$2\"\nIFS= read -r _ < \"$3\"\nexit \"$status\"\n",
    )
    .expect("write shutdown canary");
    std::fs::set_permissions(&script, Permissions::from_mode(0o700))
        .expect("make shutdown canary executable");
    let extension_command = serde_json::to_string(&[
        script.to_str().expect("UTF-8 script path").to_owned(),
        tau_bin()
            .to_str()
            .expect("UTF-8 Tau binary path")
            .to_owned(),
        stopped.to_str().expect("UTF-8 stopped path").to_owned(),
        release.to_str().expect("UTF-8 release path").to_owned(),
    ])
    .expect("serialize extension command");
    let config = root.path().join(".config/tau/harness.yaml");
    std::fs::create_dir_all(config.parent().expect("config parent")).expect("create config parent");
    std::fs::write(
        config,
        format!(
            "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n  std-notifications:\n    command: {extension_command}\n    require: true\n"
        ),
    )
    .expect("write shutdown canary config");

    let mut server = base_command(root.path())
        .args(["serve", "--session", "kill-waits", "--create"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn delayed-shutdown server");
    let server_pgid = server.id();
    wait_for_session(root.path(), "kill-waits", &mut server);
    let mut kill = command(root.path())
        .args(["session", "kill", "kill-waits"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn session kill");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !stopped.exists() {
        assert!(
            Instant::now() < deadline,
            "extension did not reach delayed shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        kill.try_wait().expect("query kill command"),
        None,
        "kill command claimed success before the daemon exited"
    );
    assert_eq!(
        server.try_wait().expect("query delayed server"),
        None,
        "shutdown canary did not keep the daemon alive"
    );

    if let Err(release_error) = release_fifo(&release, Instant::now() + RELEASE_TIMEOUT, || {
        check_shutdown_children(&mut server, &mut kill)
    }) {
        let cleanup =
            cleanup_failed_shutdown(&mut server, &mut kill, server_pgid, FAILURE_CLEANUP_GRACE);
        panic!("release extension wrapper: {release_error}; failure cleanup: {cleanup:?}");
    }
    let killed = kill.wait_with_output().expect("wait for session kill");
    assert!(
        killed.status.success(),
        "session kill failed after daemon release: {:?}",
        String::from_utf8_lossy(&killed.stderr)
    );
    wait_for_success(&mut server);
}

/// A missing reader must end at the fixture's deadline instead of blocking in
/// the FIFO writer open.
#[test]
fn release_fifo_without_reader_is_deadline_bounded() {
    let root = support::bounded_runtime_tempdir();
    let release = root.path().join("release");
    mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");

    let error = release_fifo(&release, Instant::now() + Duration::from_millis(30), || {
        Ok(())
    })
    .expect_err("missing reader must fail");
    assert!(error.contains("deadline expired"), "{error}");
}

/// An already-expired deadline must reject release before attempting a FIFO
/// open that has no reader.
#[test]
fn release_fifo_rejects_expired_deadline() {
    let root = support::bounded_runtime_tempdir();
    let release = root.path().join("release");
    mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");

    let error =
        release_fifo(&release, Instant::now(), || Ok(())).expect_err("expired release must fail");
    assert_eq!(error, "deadline expired while opening release FIFO");
}

/// A reader admitted after the first `ENXIO` retry must receive the real
/// newline-terminated release token.
#[test]
fn release_fifo_rendezvouses_with_late_reader() {
    use std::io::Read as _;
    use std::sync::mpsc;

    let root = support::bounded_runtime_tempdir();
    let release = root.path().join("release");
    mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let reader_path = release.clone();
    let reader = std::thread::spawn(move || {
        start_rx.recv().expect("receive reader start");
        let mut fifo = File::open(reader_path).expect("open release reader");
        let mut token = String::new();
        fifo.read_to_string(&mut token).expect("read release token");
        token
    });
    let mut started = false;

    release_fifo(&release, Instant::now() + Duration::from_secs(1), || {
        if !started {
            start_tx.send(()).expect("start late reader");
            started = true;
        }
        Ok(())
    })
    .expect("release late reader");
    assert_eq!(reader.join().expect("join release reader"), "release\n");
}

/// A reader that disappears after writer rendezvous must report broken pipe
/// rather than treating the release as successful.
#[test]
fn release_fifo_reports_reader_disappearance_after_open() {
    use std::sync::mpsc;

    let root = support::bounded_runtime_tempdir();
    let release = root.path().join("release");
    mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");
    let (opened_tx, opened_rx) = mpsc::sync_channel(1);
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let reader_path = release.clone();
    let reader = std::thread::spawn(move || {
        let fifo = File::open(reader_path).expect("open release reader");
        opened_tx.send(()).expect("publish reader open");
        drop_rx.recv().expect("receive reader drop");
        drop(fifo);
    });
    let mut writer = loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&release)
        {
            Ok(writer) => break writer,
            Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {
                std::thread::yield_now();
            }
            Err(error) => panic!("open release writer: {error}"),
        }
    };
    opened_rx.recv().expect("observe reader open");
    drop_tx.send(()).expect("request reader drop");
    reader.join().expect("join release reader");

    let error = write_release(&mut writer, Instant::now() + Duration::from_secs(1), || {
        Ok(())
    })
    .expect_err("disappeared reader must fail");
    assert!(error.contains("Broken pipe"), "{error}");
}

/// Forced release-error cleanup must account for the server process group and
/// reap both children without collecting piped output.
#[test]
fn release_failure_cleanup_reaps_owned_children() {
    let root = support::bounded_runtime_tempdir();
    let descendant_pid = root.path().join("descendant-pid");
    let mut server = Command::new("sh")
        .args([
            "-c",
            "sleep 30 & echo \"$!\" > \"$1\"; wait",
            "shutdown-cleanup",
        ])
        .arg(&descendant_pid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn server stand-in");
    let server_pgid = server.id();
    let mut kill = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kill-command stand-in");
    let descendant_pid = wait_for_pid_file(&descendant_pid);
    assert!(
        process_is_live(descendant_pid),
        "server descendant exited before cleanup"
    );

    cleanup_failed_shutdown(&mut server, &mut kill, server_pgid, Duration::ZERO)
        .expect("clean up failed shutdown");
    assert!(
        server.try_wait().expect("query cleaned server").is_some(),
        "server was not reaped"
    );
    assert!(
        kill.try_wait()
            .expect("query cleaned kill command")
            .is_some(),
        "kill command was not reaped"
    );
    wait_for_process_death(descendant_pid);
}

/// Failure cleanup must still kill a surviving server-group descendant after
/// both directly owned children have already exited and been reaped.
#[test]
fn release_failure_cleanup_accounts_for_wrapper_after_server_exit() {
    let root = support::bounded_runtime_tempdir();
    let descendant_pid = root.path().join("descendant-pid");
    let mut server = Command::new("sh")
        .args(["-c", "sleep 30 & echo \"$!\" > \"$1\"", "shutdown-cleanup"])
        .arg(&descendant_pid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn early-exit server stand-in");
    let server_pgid = server.id();
    let mut kill = Command::new("true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn early-exit kill-command stand-in");
    let descendant_pid = wait_for_pid_file(&descendant_pid);
    server.wait().expect("reap early-exit server");
    kill.wait().expect("reap early-exit kill command");
    assert!(
        process_is_live(descendant_pid),
        "server descendant exited before cleanup"
    );

    cleanup_failed_shutdown(&mut server, &mut kill, server_pgid, Duration::ZERO)
        .expect("clean up orphaned wrapper stand-in");
    wait_for_process_death(descendant_pid);
}

/// Waits boundedly for a shell fixture to publish one child process identifier.
fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "descendant PID was not published"
        );
        std::thread::sleep(RELEASE_RETRY_DELAY);
    }
}

/// Reports whether one Linux process still exists in a non-zombie state.
fn process_is_live(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_ascii_whitespace().next())
        != Some("Z")
}

/// Waits boundedly until a killed descendant is gone or only awaits init's
/// reap.
fn wait_for_process_death(pid: u32) {
    let deadline = Instant::now() + FORCED_REAP_TIMEOUT;
    while process_is_live(pid) {
        assert!(
            Instant::now() < deadline,
            "server-group descendant {pid} survived cleanup"
        );
        std::thread::sleep(RELEASE_RETRY_DELAY);
    }
}
