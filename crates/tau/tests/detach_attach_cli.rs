use std::fs::File;
use std::io::Write;
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
}

/// One interactive Tau child and the PTY controller used to submit commands.
struct PtyChild {
    /// Spawned foreground CLI process.
    child: Child,
    /// Parent-side PTY endpoint.
    controller: File,
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
        Self {
            child,
            controller: File::from(pty.controller),
        }
    }

    /// Submits one terminal line.
    fn line(&mut self, line: &str) {
        self.controller
            .write_all(format!("{line}\r").as_bytes())
            .expect("write PTY command");
        self.controller.flush().expect("flush PTY command");
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
