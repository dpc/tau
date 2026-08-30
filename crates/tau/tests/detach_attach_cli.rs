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

    /// Waits for policy-driven daemon shutdown to remove one discovery pair.
    fn wait_for_runtime_pair_gone(&self, metadata: &Path, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while metadata.exists() || socket.exists() {
            assert!(
                Instant::now() < deadline,
                "daemon did not remove its runtime discovery pair"
            );
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

    /// Disarms cleanup after policy-driven shutdown removed the discovery pair.
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

/// Ctrl-D on the owning initial UI disconnects without disabling the launch's
/// independent exit-on-disconnect policy, which then stops the daemon.
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
    environment.wait_for_runtime_pair_gone(&metadata, &socket);

    assert!(!metadata.exists(), "EOF left harness metadata behind");
    assert!(!socket.exists(), "EOF left harness socket behind");
    daemon.disarm();
}

/// Exact-ID creation remains discoverable across a stock attachment, pins
/// switching, handles real SIGINT cleanup, and leaves a session that the
/// unchanged strict-existing mode can subsequently resume.
#[test]
fn created_session_server_handles_stock_attach_sigint_and_strict_resume() {
    assert_created_session_server_handles_stock_attach_signal_and_strict_resume("-INT");
}

/// Exact-ID creation exercises the same complete public lifecycle under
/// SIGTERM independently from the SIGINT fixture.
#[test]
fn created_session_server_handles_stock_attach_sigterm_and_strict_resume() {
    assert_created_session_server_handles_stock_attach_signal_and_strict_resume("-TERM");
}

/// Exercises one signal through the complete create, list, PTY attach, ordered
/// extension shutdown, cleanup, and strict-resume lifecycle.
fn assert_created_session_server_handles_stock_attach_signal_and_strict_resume(signal: &str) {
    let environment = TestEnvironment::new();
    let session_id = format!("serve-{}", signal.trim_start_matches('-').to_lowercase());
    let canary = environment.configure_shutdown_canary();
    let mut server = environment
        .command()
        .args(["serve", "--session", &session_id, "--create"])
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

    let mut resumed = environment
        .command()
        .args(["serve", "--session", &session_id, "--existing"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("strictly resume created session");
    let resumed_metadata = environment.wait_for_metadata();
    let resumed_socket = resumed_metadata.with_extension("sock");
    let signaled = Command::new("kill")
        .args(["-TERM", &resumed.id().to_string()])
        .status()
        .expect("signal resumed foreground server");
    assert!(signaled.success(), "signal resumed foreground server");
    let status = resumed.wait().expect("reap resumed foreground server");
    assert!(status.success(), "strict existing resume failed: {status}");
    assert!(!resumed_metadata.exists(), "resumed server left metadata");
    assert!(!resumed_socket.exists(), "resumed server left socket");
    environment.assert_runtime_discovery_empty();
}

/// The public serve CLI admits one bootstrap generation, remains attachable,
/// skips the same id before touching a missing source, and creates a new agent
/// only when the operator supplies a different id.
#[test]
fn serve_bootstrap_is_durable_at_most_once_across_real_restarts() {
    fn wait_for_agents(environment: &TestEnvironment, session_id: &str, count: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = environment
                .command()
                .args(["agent", "list", session_id])
                .output()
                .expect("list bootstrap agents");
            let rows = String::from_utf8(output.stdout).expect("UTF-8 agent rows");
            if output.status.success() && rows.lines().count() == count {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "bootstrap agent count did not reach {count}: {rows:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop_server(
        environment: &TestEnvironment,
        server: Child,
        metadata: &Path,
        socket: &Path,
    ) -> std::process::Output {
        assert!(
            Command::new("kill")
                .args(["-TERM", &server.id().to_string()])
                .status()
                .expect("signal bootstrap server")
                .success()
        );
        let output = server.wait_with_output().expect("wait bootstrap server");
        assert!(output.status.success());
        environment.wait_for_runtime_pair_gone(metadata, socket);
        output
    }

    let environment = TestEnvironment::new();
    let session_id = "serve-bootstrap";
    let source = environment.temp.path().join("bootstrap.prompt");
    let secret = "bootstrap-secret-27e8bca041";
    std::fs::write(&source, secret).expect("write bootstrap prompt");
    let source_arg = source.to_string_lossy().into_owned();

    let first = environment
        .command()
        .args([
            "serve",
            "--session",
            session_id,
            "--create",
            "--bootstrap-prompt-file",
            &source_arg,
            "--bootstrap-id",
            "assistant-v1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start bootstrapped server");
    let first_metadata = environment.wait_for_metadata();
    let first_socket = first_metadata.with_extension("sock");
    let first_rows = wait_for_agents(&environment, session_id, 1);
    let first_output = stop_server(&environment, first, &first_metadata, &first_socket);
    assert!(!String::from_utf8_lossy(&first_output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&first_output.stderr).contains(secret));
    let debug_log = environment
        .state_home
        .join(format!("tau/sessions/{session_id}/events.jsonl"));
    let debug_jsonl = std::fs::read_to_string(debug_log).expect("read bootstrap debug JSONL");
    assert!(!debug_jsonl.contains(secret));
    assert!(
        debug_jsonl.contains("\"event_name\":\"agent.prompt_queued\""),
        "test must force the sensitive queued projection"
    );
    let agent_id = first_rows.split('\t').next().expect("bootstrap agent id");
    let trace = environment
        .command()
        .args(["agent", "trace", agent_id, "--format", "tau-jsonl"])
        .output()
        .expect("read canonical bootstrap transcript");
    assert!(
        trace.status.success(),
        "agent trace failed: {}",
        String::from_utf8_lossy(&trace.stderr)
    );
    let trace = String::from_utf8(trace.stdout).expect("UTF-8 agent trace");
    assert!(trace.contains("engineer"), "role was not durable: {trace}");
    assert!(
        trace.contains("tau.bootstrap_prompt") && trace.contains("assistant-v1"),
        "canonical trace lost the bootstrap marker: {trace}"
    );

    std::fs::remove_file(&source).expect("remove bootstrap source");
    let restart = environment
        .command()
        .args([
            "serve",
            "--session",
            session_id,
            "--existing",
            "--bootstrap-prompt-file",
            &source_arg,
            "--bootstrap-id",
            "assistant-v1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("restart bootstrapped server");
    let restart_metadata = environment.wait_for_metadata();
    let restart_socket = restart_metadata.with_extension("sock");
    let restart_rows = wait_for_agents(&environment, session_id, 1);
    assert_eq!(
        restart_rows.split('\t').next(),
        first_rows.split('\t').next(),
        "same bootstrap id created another agent"
    );
    let _ = stop_server(&environment, restart, &restart_metadata, &restart_socket);

    std::fs::write(&source, "Follow your updated instructions.")
        .expect("write next bootstrap prompt");
    let next = environment
        .command()
        .args([
            "serve",
            "--session",
            session_id,
            "--existing",
            "--bootstrap-prompt-file",
            &source_arg,
            "--bootstrap-id",
            "assistant-v2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start next bootstrap generation");
    let next_metadata = environment.wait_for_metadata();
    let next_socket = next_metadata.with_extension("sock");
    let _ = wait_for_agents(&environment, session_id, 2);
    let _ = stop_server(&environment, next, &next_metadata, &next_socket);
}

/// A bootstrap reader waiting for stdin EOF does not own serve lifecycle:
/// SIGTERM still reaches the event loop and performs ordinary cleanup.
#[test]
fn serve_bootstrap_stdin_wait_is_signal_interruptible() {
    let environment = TestEnvironment::new();
    let mut server = environment
        .command()
        .args([
            "serve",
            "--session",
            "serve-bootstrap-stdin",
            "--create",
            "--bootstrap-prompt-file",
            "-",
            "--bootstrap-id",
            "stdin-v1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start stdin bootstrap server");
    let _stdin_guard = server.stdin.take().expect("retain bootstrap stdin");
    let metadata = environment.wait_for_metadata();
    let socket = metadata.with_extension("sock");
    let probe = environment
        .command()
        .args(["agent", "list", "serve-bootstrap-stdin"])
        .output()
        .expect("probe stdin bootstrap server");
    assert!(
        probe.status.success(),
        "event loop did not become attachable"
    );
    assert!(
        Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .expect("signal stdin bootstrap server")
            .success()
    );
    let output = server
        .wait_with_output()
        .expect("wait stdin bootstrap server");
    assert!(
        output.status.success(),
        "stdin bootstrap signal exit: {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    environment.wait_for_runtime_pair_gone(&metadata, &socket);
}

/// SIGTERM wins cleanly after the bootstrap worker has read its source and
/// connected its private client but before any create result can exist.
#[test]
fn serve_bootstrap_connected_wait_is_signal_interruptible() {
    let environment = TestEnvironment::new();
    let source = environment.temp.path().join("connected.prompt");
    std::fs::write(&source, "connected-race-secret").expect("write bootstrap prompt");
    let barrier = environment.temp.path().join("bootstrap-barrier");
    let connected = barrier.with_extension("connected");
    let server = environment
        .command()
        .args([
            "serve",
            "--session",
            "serve-bootstrap-connected",
            "--create",
            "--bootstrap-prompt-file",
            source.to_str().expect("UTF-8 prompt path"),
            "--bootstrap-id",
            "connected-v1",
        ])
        .env("TAU_TEST_BOOTSTRAP_CONNECTED_BARRIER", &barrier)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start connected bootstrap server");
    let metadata = environment.wait_for_metadata();
    let socket = metadata.with_extension("sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !connected.exists() {
        assert!(
            Instant::now() < deadline,
            "bootstrap client did not reach connected barrier"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .expect("signal connected bootstrap server")
            .success()
    );
    let output = server
        .wait_with_output()
        .expect("wait connected bootstrap server");
    assert!(
        output.status.success(),
        "connected bootstrap signal exit: {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    environment.wait_for_runtime_pair_gone(&metadata, &socket);
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
    let locked_session = locked.state_home.join("tau/sessions/locked");
    let lock_path = locked_session.join("lock");
    let canonical_before = ["meta.json", "events.cbor", "restore-events.cbor"].map(|name| {
        (
            name,
            std::fs::read(locked_session.join(name)).expect("snapshot locked canonical state"),
        )
    });
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

    let output = locked
        .command()
        .args(["serve", "--session", "locked", "--create"])
        .output()
        .expect("reject create over locked session");
    assert!(!output.status.success());
    locked.assert_runtime_discovery_empty();
    for (name, before) in canonical_before {
        assert_eq!(
            std::fs::read(locked_session.join(name)).expect("read locked canonical state"),
            before,
            "create mode mutated locked {name}"
        );
    }

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

/// Create mode rejects a valid existing session and preserves its canonical
/// manifest byte for byte without publishing runtime discovery.
#[test]
fn create_session_server_rejects_valid_existing_session_without_mutation() {
    let valid = TestEnvironment::new();
    valid.provision_session("valid");
    let valid_meta = valid.state_home.join("tau/sessions/valid/meta.json");
    let valid_before = std::fs::read(&valid_meta).expect("valid manifest");
    let output = valid
        .command()
        .args(["serve", "--session", "valid", "--create"])
        .output()
        .expect("reject valid existing session");
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(valid_meta).expect("preserved valid manifest"),
        valid_before
    );
    valid.assert_runtime_discovery_empty();
}

/// Create mode preserves a malformed manifest instead of claiming its
/// pre-existing session directory.
#[test]
fn create_session_server_preserves_malformed_manifest() {
    assert_create_session_server_preserves_partial_state(
        "malformed-create",
        "meta.json",
        b"{not-json",
    );
}

/// Create mode preserves a partial canonical journal instead of claiming its
/// pre-existing session directory.
#[test]
fn create_session_server_preserves_partial_journal() {
    assert_create_session_server_preserves_partial_state(
        "partial-create",
        "events.cbor",
        b"partial",
    );
}

/// Create mode treats a diagnostic-only directory as pre-existing authority
/// and preserves its artifact.
#[test]
fn create_session_server_preserves_diagnostic_only_state() {
    assert_create_session_server_preserves_partial_state(
        "diagnostic-create",
        "logs/extension.log",
        b"diagnostic",
    );
}

/// Exercises one pre-existing directory shape through the public create-mode
/// rejection and verifies byte-for-byte preservation without runtime leakage.
fn assert_create_session_server_preserves_partial_state(
    session_id: &str,
    relative_path: &str,
    bytes: &[u8],
) {
    let environment = TestEnvironment::new();
    let session = environment
        .state_home
        .join(format!("tau/sessions/{session_id}"));
    let artifact = session.join(relative_path);
    std::fs::create_dir_all(artifact.parent().expect("artifact parent"))
        .expect("create partial session");
    std::fs::write(&artifact, bytes).expect("write partial session artifact");
    let output = environment
        .command()
        .args(["serve", "--session", session_id, "--create"])
        .output()
        .expect("reject partial session");
    assert!(
        !output.status.success(),
        "{session_id} unexpectedly created"
    );
    assert_eq!(
        std::fs::read(&artifact).expect("preserved partial artifact"),
        bytes,
        "{session_id} mutated its existing artifact"
    );
    environment.assert_runtime_discovery_empty();
}
