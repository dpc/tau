use std::fs::{File, Permissions};
use std::io::{Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix_openpty::rustix::termios::Winsize;

/// Isolated process environment shared by every CLI in one lifecycle test.
struct TestEnvironment {
    /// Scratch root that owns every test artifact.
    temp: tempfile::TempDir,
    /// Isolated configuration home.
    config_home: PathBuf,
    /// Isolated persistent state home.
    state_home: PathBuf,
    /// Isolated live runtime directory.
    runtime_dir: PathBuf,
}

struct ShutdownCanary {
    stopped: PathBuf,
    release: PathBuf,
}

impl TestEnvironment {
    /// Creates a minimal configuration with every bundled extension disabled.
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary test root");
        let config_home = temp.path().join("config");
        let state_home = temp.path().join("state");
        let runtime_dir = temp.path().join("runtime");
        std::fs::create_dir_all(config_home.join("tau")).expect("create config directory");
        std::fs::create_dir_all(&state_home).expect("create state directory");
        std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
        std::fs::write(
            config_home.join("tau/harness.yaml"),
            "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n",
        )
        .expect("write minimal harness configuration");
        Self {
            temp,
            config_home,
            state_home,
            runtime_dir,
        }
    }

    /// Builds one public Tau command without inherited user configuration.
    fn command(&self) -> Command {
        let mut command =
            Command::new(std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau"));
        command
            .env_clear()
            .env("HOME", self.temp.path().join("home"))
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("LANG", "C.UTF-8")
            .env("TERM", "xterm-256color");
        command
    }

    /// Returns the sole runtime metadata path once the harness publishes it.
    fn wait_for_metadata(&self) -> PathBuf {
        let harnesses = self.runtime_dir.join("tau/harnesses");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let metadata = std::fs::read_dir(&harnesses).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "json")
                    })
            });
            if let Some(metadata) = metadata {
                return metadata;
            }
            assert!(
                Instant::now() < deadline,
                "harness metadata was not published"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the requested number of UI processes have entered input.
    fn wait_for_ready_uis(&self, expected: usize) {
        let ui_root = self.state_home.join("tau/uis");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ready = std::fs::read_dir(&ui_root)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path().join("ui.log")).ok())
                .filter(|log| log.contains("terminal UI input ready"))
                .count();
            if expected <= ready {
                return;
            }
            assert!(Instant::now() < deadline, "UI input did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Provisions one durable session through the real component entrypoint.
    fn provision_session(&self, session_id: &str) {
        let mut child = self
            .command()
            .args(["component", "harness"])
            .env("TAU_SESSION_ID", session_id)
            .env("TAU_SESSION_STATUS", "new")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn provisioning harness");
        let _metadata = self.wait_for_metadata();
        let status = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("signal provisioning harness");
        assert!(status.success(), "signal provisioning harness");
        let _ = child.wait().expect("reap provisioning harness");
        std::fs::remove_dir_all(self.runtime_dir.join("tau"))
            .expect("remove provisioning runtime files");
    }

    /// Enables a real bundled extension behind a wrapper that exposes and
    /// pauses its shutdown boundary.
    fn configure_shutdown_canary(&self) -> ShutdownCanary {
        let script = self.temp.path().join("shutdown-canary");
        let stopped = self.temp.path().join("extension-stopped");
        let release = self.temp.path().join("release-extension-wrapper");
        std::fs::write(
            &script,
            "#!/bin/sh\n\"$1\" component ext-std-notifications\nstatus=$?\nprintf stopped > \"$2\"\nwhile [ ! -e \"$3\" ]; do sleep 0.01; done\nexit \"$status\"\n",
        )
        .expect("write shutdown canary");
        std::fs::set_permissions(&script, Permissions::from_mode(0o700))
            .expect("make shutdown canary executable");
        let command = serde_json::to_string(&[
            script.to_str().expect("UTF-8 canary path").to_owned(),
            std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau"),
            stopped.to_str().expect("UTF-8 stopped path").to_owned(),
            release.to_str().expect("UTF-8 release path").to_owned(),
        ])
        .expect("serialize canary command");
        std::fs::write(
            self.config_home.join("tau/harness.yaml"),
            format!(
                "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n  std-notifications:\n    command: {command}\n    require: true\n    tau_runtime_socket_access: legacy\n"
            ),
        )
        .expect("configure shutdown canary");
        ShutdownCanary { stopped, release }
    }

    /// Requires the runtime harness directory to contain no socket or metadata.
    fn assert_runtime_discovery_empty(&self) {
        let harnesses = self.runtime_dir.join("tau/harnesses");
        let count = std::fs::read_dir(harnesses)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .count();
        assert_eq!(count, 0, "runtime discovery files leaked");
    }
}

/// One interactive Tau child and the PTY controller used to submit commands.
struct PtyChild {
    /// Spawned foreground CLI process.
    child: Child,
    /// Parent-side PTY endpoint.
    controller: File,
    /// Terminal output copied by one detached reader.
    output: mpsc::Receiver<Vec<u8>>,
    /// Output accumulated by synchronization assertions.
    accumulated_output: Vec<u8>,
}

/// Best-effort cleanup for a daemon intentionally detached by the test.
struct DetachedDaemonGuard {
    /// Process identifier parsed from Tau's runtime metadata name.
    pid: String,
    /// Whether unwind cleanup must still stop the daemon.
    armed: bool,
}

impl DetachedDaemonGuard {
    /// Tracks the daemon owning one runtime metadata path.
    fn from_metadata(metadata: &Path) -> Self {
        let pid = metadata
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.split_once('-'))
            .map(|(pid, _)| pid)
            .expect("metadata process id")
            .to_owned();
        Self { pid, armed: true }
    }

    /// Disarms cleanup after a stop-owned exit has already reaped the daemon.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DetachedDaemonGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = Command::new("kill").arg("-KILL").arg(&self.pid).status();
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl PtyChild {
    /// Starts a Tau command with all three standard streams attached to one
    /// PTY.
    fn spawn(mut command: Command) -> Self {
        let pty = rustix_openpty::openpty(
            None,
            Some(&Winsize {
                ws_row: 24,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
        )
        .expect("open PTY");
        command
            .stdin(Stdio::from(pty.user.try_clone().expect("clone PTY input")))
            .stdout(Stdio::from(pty.user.try_clone().expect("clone PTY output")))
            .stderr(Stdio::from(pty.user));
        let child = command.spawn().expect("spawn interactive tau");
        let controller = File::from(pty.controller);
        let mut output_reader = controller.try_clone().expect("clone PTY output");
        let (output_tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match output_reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if output_tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            controller,
            output,
            accumulated_output: Vec::new(),
        }
    }

    /// Submits one terminal line.
    fn line(&mut self, line: &str) {
        self.controller
            .write_all(format!("{line}\r").as_bytes())
            .expect("write PTY command");
        self.controller.flush().expect("flush PTY command");
    }

    /// Drops already-rendered bytes before one causal terminal assertion.
    fn clear_output(&mut self) {
        self.accumulated_output.clear();
        while self.output.try_recv().is_ok() {}
    }

    /// Waits until terminal output after the last clear contains exact text.
    fn wait_for_text(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self
            .accumulated_output
            .windows(text.len())
            .any(|window| window == text.as_bytes())
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "terminal did not render `{text}`");
            let bytes = self
                .output
                .recv_timeout(remaining)
                .expect("terminal output before deadline");
            self.accumulated_output.extend(bytes);
        }
    }

    /// Requires the CLI to exit successfully within a tight lifecycle bound.
    fn wait_success(mut self, state_home: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait().expect("query CLI process") {
                Some(status) => {
                    assert!(status.success(), "interactive CLI failed: {status}");
                    return;
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    if let Ok(entries) = std::fs::read_dir(state_home.join("tau/uis")) {
                        for entry in entries.filter_map(Result::ok) {
                            if let Ok(log) = std::fs::read_to_string(entry.path().join("ui.log")) {
                                eprintln!("{log}");
                            }
                        }
                    }
                    let tasks = PathBuf::from(format!("/proc/{}/task", self.child.id()));
                    if let Ok(entries) = std::fs::read_dir(tasks) {
                        for entry in entries.filter_map(Result::ok) {
                            let wchan = std::fs::read_to_string(entry.path().join("wchan"))
                                .unwrap_or_default();
                            eprintln!("task {}: {wchan}", entry.file_name().to_string_lossy());
                        }
                    }
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("interactive CLI did not exit within two seconds");
                }
            }
        }
    }
}

/// `:detach` from the CLI-owned initial transport must return promptly while
/// the same daemon remains discoverable and accepts repeated real PTY
/// attachments. A concurrent reader keeps parsing the runtime metadata
/// throughout those handoffs to verify observers retain a complete discovery
/// document while UI ownership changes.
#[test]
fn owned_cli_detaches_and_repeatedly_reattaches_to_same_daemon() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let metadata = environment.wait_for_metadata();
    let _daemon = DetachedDaemonGuard::from_metadata(&metadata);
    environment.wait_for_ready_uis(1);
    let metadata_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata).expect("read metadata"))
            .expect("parse metadata");
    let session_id = metadata_value["session_id"]
        .as_str()
        .expect("metadata session id")
        .to_owned();
    let socket = metadata.with_extension("sock");
    let (stop_reader_tx, stop_reader_rx) = mpsc::channel();
    let (reader_ready_tx, reader_ready_rx) = mpsc::sync_channel(0);
    let observed_metadata = metadata.clone();
    let observed_session_id = session_id.clone();
    let metadata_reader = std::thread::spawn(move || {
        let initial = std::fs::read(&observed_metadata).expect("initial live daemon metadata");
        serde_json::from_slice::<serde_json::Value>(&initial)
            .expect("initial live daemon metadata is complete");
        reader_ready_tx
            .send(())
            .expect("report active metadata reader");
        while stop_reader_rx.try_recv().is_err() {
            let encoded =
                std::fs::read(&observed_metadata).expect("live daemon metadata remains readable");
            let value: serde_json::Value =
                serde_json::from_slice(&encoded).expect("live daemon metadata remains complete");
            assert_eq!(value["session_id"], observed_session_id);
            std::thread::yield_now();
        }
    });
    reader_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("metadata reader became active");

    owner.line(":detach");
    owner.wait_success(&environment.state_home);
    assert!(metadata.exists(), "detach removed live metadata");
    assert!(socket.exists(), "detach removed live socket");

    for cycle in 0..3 {
        let output = environment
            .command()
            .args(["session", "list", "--json"])
            .output()
            .expect("list running sessions");
        assert!(output.status.success(), "session list failed");
        let listed: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("parse session list");
        assert!(
            listed
                .as_array()
                .expect("session list array")
                .iter()
                .any(|row| row["session_id"] == session_id),
            "detached session was not discoverable: {listed}"
        );

        let mut attached = environment.command();
        attached.args(["attach", &session_id]);
        let mut attached = PtyChild::spawn(attached);
        environment.wait_for_ready_uis(cycle + 2);
        attached.line(":detach");
        attached.wait_success(&environment.state_home);
        assert!(metadata.exists(), "reattach cycle replaced daemon metadata");
        assert!(socket.exists(), "reattach cycle removed daemon socket");
    }
    let _ = stop_reader_tx.send(());
    metadata_reader.join().expect("concurrent metadata reader");
}

/// Ctrl-D on the owning initial UI remains a stop-owned exit rather than being
/// silently reclassified as detach.
#[test]
fn owned_cli_eof_stops_daemon_and_removes_discovery_pair() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let metadata = environment.wait_for_metadata();
    let mut daemon = DetachedDaemonGuard::from_metadata(&metadata);
    environment.wait_for_ready_uis(1);
    let socket = metadata.with_extension("sock");

    owner
        .controller
        .write_all(&[4])
        .expect("write terminal EOF");
    owner.controller.flush().expect("flush terminal EOF");
    owner.wait_success(&environment.state_home);

    assert!(!metadata.exists(), "EOF left harness metadata behind");
    assert!(!socket.exists(), "EOF left harness socket behind");
    daemon.disarm();
}

/// The public foreground server remains discoverable across a stock attachment,
/// then actual SIGINT/SIGTERM requests complete ordered cleanup with exit zero.
#[test]
fn existing_session_server_handles_stock_attach_and_real_signals() {
    for signal in ["-INT", "-TERM"] {
        let environment = TestEnvironment::new();
        let session_id = format!("serve-{}", signal.trim_start_matches('-').to_lowercase());
        environment.provision_session(&session_id);
        let canary = environment.configure_shutdown_canary();
        let mut server = environment
            .command()
            .args(["serve", "--session", &session_id, "--existing"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn foreground server");
        let metadata = environment.wait_for_metadata();
        let socket = metadata.with_extension("sock");
        let children_path = PathBuf::from(format!(
            "/proc/{}/task/{}/children",
            server.id(),
            server.id()
        ));
        let child_pids = std::fs::read_to_string(&children_path)
            .expect("read supervised extension children")
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        assert!(
            !child_pids.is_empty(),
            "serve acceptance must exercise supervised extension teardown"
        );

        let listed = environment
            .command()
            .args(["session", "list", "--json"])
            .output()
            .expect("list served session");
        assert!(listed.status.success(), "session list failed");
        assert!(String::from_utf8_lossy(&listed.stdout).contains(&session_id));

        let mut attach = environment.command();
        attach.args(["attach", &session_id]);
        let mut attach = PtyChild::spawn(attach);
        environment.wait_for_ready_uis(1);
        attach.clear_output();
        attach.line(":session new");
        attach.wait_for_text("pinned by the foreground server");
        attach.wait_for_text(&format!("&{session_id}"));
        attach.line(":cancel");
        let events_path = environment
            .state_home
            .join(format!("tau/sessions/{session_id}/events.jsonl"));
        let cancel_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let routed_to_pinned_session = std::fs::read_to_string(&events_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|row| {
                    row.get("type").and_then(serde_json::Value::as_str) == Some("from_connection")
                        && row
                            .pointer("/event/message")
                            .and_then(serde_json::Value::as_str)
                            == Some("emit")
                        && row
                            .pointer("/event/payload/event/event")
                            .and_then(serde_json::Value::as_str)
                            == Some("ui.cancel_prompt")
                        && row
                            .pointer("/event/payload/event/payload/session_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(session_id.as_str())
                });
            if routed_to_pinned_session {
                break;
            }
            assert!(
                Instant::now() < cancel_deadline,
                "stock attachment did not route cancel to pinned session"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        attach.line(":detach");
        attach.wait_success(&environment.state_home);
        assert!(metadata.exists(), "attachment disconnect removed metadata");
        assert!(socket.exists(), "attachment disconnect removed socket");

        let signaled = Command::new("kill")
            .args([signal, &server.id().to_string()])
            .status()
            .expect("signal foreground server");
        assert!(signaled.success(), "signal foreground server");
        let admission_deadline = Instant::now() + Duration::from_secs(2);
        while UnixStream::connect(&socket).is_ok() {
            assert!(
                Instant::now() < admission_deadline,
                "signal did not retire listener admission"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            metadata.exists(),
            "runtime metadata retired before extension shutdown"
        );
        let extension_deadline = Instant::now() + Duration::from_secs(2);
        while !canary.stopped.exists() {
            assert!(
                Instant::now() < extension_deadline,
                "extension did not observe ordered shutdown"
            );
            assert!(
                metadata.exists(),
                "runtime metadata retired before extension stopped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            metadata.exists(),
            "runtime metadata retired before extension wrapper was reaped"
        );
        std::fs::write(&canary.release, b"release").expect("release extension wrapper");
        let status = server.wait().expect("reap foreground server");
        assert!(status.success(), "foreground server signal exit: {status}");
        assert!(!metadata.exists(), "signal left metadata");
        assert!(!socket.exists(), "signal left socket");
        assert!(
            child_pids
                .iter()
                .all(|pid| !PathBuf::from(format!("/proc/{pid}")).exists()),
            "signal exit left a supervised extension child"
        );
        environment.assert_runtime_discovery_empty();
    }
}

/// Existing-only startup rejects incompatible one-shot modes and every strict
/// persistence failure without publishing or leaking runtime discovery files.
#[test]
fn existing_session_server_rejects_invalid_modes_and_strict_state_failures() {
    for arguments in [
        vec!["--ephemeral", "serve", "--session", "missing", "--existing"],
        vec![
            "--prompt-stdin",
            "serve",
            "--session",
            "missing",
            "--existing",
        ],
        vec!["serve", "--session", "missing", "--existing"],
    ] {
        let environment = TestEnvironment::new();
        let output = environment
            .command()
            .args(arguments)
            .output()
            .expect("run rejected server");
        assert!(!output.status.success());
        environment.assert_runtime_discovery_empty();
    }

    let locked = TestEnvironment::new();
    locked.provision_session("locked");
    let lock_path = locked.state_home.join("tau/sessions/locked/lock");
    let lock = File::options()
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("open session lock");
    fs2::FileExt::lock_exclusive(&lock).expect("lock session");
    let output = locked
        .command()
        .args(["serve", "--session", "locked", "--existing"])
        .output()
        .expect("run locked server");
    assert!(!output.status.success());
    locked.assert_runtime_discovery_empty();

    let malformed = TestEnvironment::new();
    malformed.provision_session("malformed");
    std::fs::write(
        malformed
            .state_home
            .join("tau/sessions/malformed/events.cbor"),
        b"malformed",
    )
    .expect("corrupt session journal");
    let output = malformed
        .command()
        .args(["serve", "--session", "malformed", "--existing"])
        .output()
        .expect("run malformed server");
    assert!(!output.status.success());
    malformed.assert_runtime_discovery_empty();
}
