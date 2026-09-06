//! Hermetic lifecycle coverage for `tau session kill`.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

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

/// Waits until session discovery reports every requested exact identifier.
fn wait_for_sessions(root: &Path, expected: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = command(root)
            .args(["session", "list"])
            .output()
            .expect("list sessions");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && expected
                .iter()
                .all(|id| stdout.lines().any(|line| line == *id))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sessions did not become discoverable; stdout={stdout:?}, stderr={:?}",
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

/// Exact-session kill uses canonical shutdown, leaves another daemon alone,
/// rejects absence, retires runtime discovery, and preserves durable history.
#[test]
fn exact_session_kill_preserves_history_and_other_sessions() {
    let root = support::bounded_runtime_tempdir();
    let mut target = spawn_session(root.path(), "kill-target");
    let mut other = spawn_session(root.path(), "kill-other");
    wait_for_sessions(root.path(), &["kill-target", "kill-other"]);

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
    wait_for_sessions(root.path(), &["kill-inaccessible"]);
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

    use nix::sys::stat::Mode;

    let root = support::bounded_runtime_tempdir();
    let _ = base_command(root.path());
    let script = root.path().join("shutdown-canary");
    let stopped = root.path().join("extension-stopped");
    let release = root.path().join("release-extension-wrapper");
    nix::unistd::mkfifo(&release, Mode::S_IRUSR | Mode::S_IWUSR).expect("create release FIFO");
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
        .spawn()
        .expect("spawn delayed-shutdown server");
    wait_for_sessions(root.path(), &["kill-waits"]);
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

    std::fs::write(&release, b"release\n").expect("release extension wrapper");
    let killed = kill.wait_with_output().expect("wait for session kill");
    assert!(
        killed.status.success(),
        "session kill failed after daemon release: {:?}",
        String::from_utf8_lossy(&killed.stderr)
    );
    wait_for_success(&mut server);
}
