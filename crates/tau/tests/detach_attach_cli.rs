use std::fs::{File, Permissions};
use std::io::{BufRead as _, BufReader, Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix_openpty::rustix::termios::Winsize;
use rustix_v1::process::{Pid, Signal, kill_process};

#[path = "detach_attach_cli/owned_process_group.rs"]
mod owned_process_group;

use owned_process_group::{
    OwnedProcessGroup, child_reap_poll_survives_transient_none, group_exists,
    run_watchdog_worker_from_env, watchdog_confirms_matching_anchor_stopped,
    watchdog_rejects_mismatched_anchor, watchdog_waits_for_root_only_tracked_identity,
};

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

    /// Configures one real extension wrapper that emits arbitrary stderr before
    /// delegating to a stock protocol-speaking component.
    fn configure_stderr_canary(&self, canary: &str) -> ShutdownCanary {
        let script = self.temp.path().join("stderr-canary");
        let stopped = self.temp.path().join("stderr-extension-stopped");
        let release = self.temp.path().join("release-stderr-extension");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$4\" >&2\n\"$1\" component ext-std-notifications\nstatus=$?\nprintf stopped > \"$2\"\nwhile [ ! -e \"$3\" ]; do sleep 0.01; done\nexit \"$status\"\n",
        )
        .expect("write stderr canary");
        std::fs::set_permissions(&script, Permissions::from_mode(0o700))
            .expect("make stderr canary executable");
        let command = serde_json::to_string(&[
            script.to_str().expect("UTF-8 canary path").to_owned(),
            std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau"),
            stopped.to_str().expect("UTF-8 stopped path").to_owned(),
            release.to_str().expect("UTF-8 release path").to_owned(),
            canary.to_owned(),
        ])
        .expect("serialize stderr canary command");
        std::fs::write(
            self.config_home.join("tau/harness.yaml"),
            format!(
                "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n  std-notifications:\n    command: {command}\n    require: true\n    tau_runtime_socket_access: legacy\n"
            ),
        )
        .expect("configure stderr canary");
        ShutdownCanary { stopped, release }
    }

    /// Configures a first child whose inherited stderr stays open while the
    /// harness starts a second generation of the same extension.
    fn configure_respawn_stderr_overlap(&self) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let script = self.temp.path().join("respawn-stderr-overlap");
        let first_started = self.temp.path().join("respawn-first-started");
        let release_protocol = self.temp.path().join("respawn-release-protocol");
        let release_old = self.temp.path().join("respawn-release-old-stderr");
        let second_started = self.temp.path().join("respawn-second-started");
        let old_written = self.temp.path().join("respawn-old-written");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             if [ ! -e \"$2\" ]; then\n\
               printf '%s' \"$$\" > \"$2\"\n\
               (while [ ! -e \"$3\" ]; do sleep 0.01; done; printf 'stdout-protocol-private') 2>/dev/null &\n\
               (while [ ! -e \"$4\" ]; do sleep 0.01; done; printf 'old-late\\n' >&2; : > \"$6\") >/dev/null &\n\
               exec \"$1\" component ext-std-notifications\n\
             fi\n\
             printf 'new-start\\n' >&2\n\
             printf '%s' \"$$\" > \"$5\"\n\
             exec \"$1\" component ext-std-notifications\n",
        )
        .expect("write respawn overlap wrapper");
        std::fs::set_permissions(&script, Permissions::from_mode(0o700))
            .expect("make respawn overlap wrapper executable");
        let command = serde_json::to_string(&[
            script.to_str().expect("UTF-8 wrapper path").to_owned(),
            std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau"),
            first_started
                .to_str()
                .expect("UTF-8 marker path")
                .to_owned(),
            release_protocol
                .to_str()
                .expect("UTF-8 marker path")
                .to_owned(),
            release_old.to_str().expect("UTF-8 marker path").to_owned(),
            second_started
                .to_str()
                .expect("UTF-8 marker path")
                .to_owned(),
            old_written.to_str().expect("UTF-8 marker path").to_owned(),
        ])
        .expect("serialize respawn overlap command");
        std::fs::write(
            self.config_home.join("tau/harness.yaml"),
            format!(
                "extensions:\n  provider-builtin:\n    enable: false\n  core-shell:\n    enable: false\n  std-notifications:\n    command: {command}\n    require: true\n    tau_runtime_socket_access: legacy\n"
            ),
        )
        .expect("configure respawn overlap");
        (
            first_started,
            release_protocol,
            release_old,
            second_started,
            old_written,
        )
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

/// Nonterminal fixed-session serve mirrors arbitrary extension stderr only
/// when explicitly requested, retains the raw private file, and cleans up on
/// SIGTERM without duplicate mirror records.
#[test]
fn fixed_session_serve_mirrors_extension_stderr_only_when_enabled() {
    for enabled in [false, true] {
        let environment = TestEnvironment::new();
        let canary = format!("custom-stderr-canary-enabled-{enabled}");
        let shutdown = environment.configure_stderr_canary(&canary);
        let session_id = format!("stderr-mirror-{enabled}");
        let mut command = environment.command();
        command.args(["serve", "--session", &session_id, "--create"]);
        if enabled {
            command.arg("--mirror-extension-stderr");
        }
        let server = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stderr mirror server");
        let _metadata = environment.wait_for_metadata();
        Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .expect("signal stderr mirror server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !shutdown.stopped.exists() {
            assert!(
                Instant::now() < deadline,
                "extension did not stop after SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(&shutdown.release, b"release").expect("release extension wrapper");
        let output = server
            .wait_with_output()
            .expect("wait for stderr mirror server");
        assert!(output.status.success(), "serve exited unsuccessfully");
        let stderr = String::from_utf8(output.stderr).expect("serve stderr is UTF-8");
        assert_eq!(
            stderr.matches(&canary).count(),
            usize::from(enabled),
            "extension stderr mirror opt-in or exactly-once behavior changed"
        );
        assert!(
            stderr.contains("extension spawned"),
            "ordinary harness tracing must remain on process stderr"
        );
        let raw_path = environment.state_home.join(format!(
            "tau/sessions/{session_id}/logs/std-notifications.log"
        ));
        let raw = std::fs::read(&raw_path).expect("read authoritative extension log");
        assert!(
            raw.windows(canary.len())
                .any(|window| window == canary.as_bytes()),
            "authoritative raw extension log lost child bytes"
        );
        assert!(
            String::from_utf8_lossy(&raw).contains("attached at"),
            "existing raw attach marker changed"
        );
        assert!(
            !stderr.contains("attached at"),
            "private-file markers must never be mirrored"
        );
        environment.assert_runtime_discovery_empty();
    }
}

/// An explicitly empty producer filter suppresses harness and built-in tracing
/// without suppressing one arbitrary custom stderr record from the mirror.
#[test]
fn fixed_session_stderr_mirror_does_not_reinterpret_empty_tau_log() {
    let environment = TestEnvironment::new();
    let canary = "custom-debug-canary-exactly-once";
    let shutdown = environment.configure_stderr_canary(canary);
    let session_id = "stderr-mirror-empty-tau-log";
    let server = environment
        .command()
        .env("TAU_LOG", "")
        .args([
            "serve",
            "--session",
            session_id,
            "--create",
            "--mirror-extension-stderr",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn empty-filter stderr mirror server");
    let _metadata = environment.wait_for_metadata();
    Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()
        .expect("signal empty-filter server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shutdown.stopped.exists() {
        assert!(
            Instant::now() < deadline,
            "empty-filter extension did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(&shutdown.release, b"release").expect("release empty-filter wrapper");
    let output = server
        .wait_with_output()
        .expect("wait for empty-filter server");
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("serve stderr is UTF-8");
    assert_eq!(stderr.matches(canary).count(), 1);
    assert!(
        !stderr.contains("extension spawned"),
        "empty TAU_LOG must continue suppressing harness tracing"
    );
    assert!(
        stderr.contains("extension=std-notifications generation=0 pid="),
        "custom stderr record lost immutable attribution"
    );
}

/// A real supervised respawn keeps old and new same-name stderr loggers
/// separately attributed even when the old pipe writes after generation one
/// starts.
#[test]
fn fixed_session_stderr_mirror_attributes_real_respawn_overlap() {
    let environment = TestEnvironment::new();
    let (first_pid_path, release_protocol, release_old, second_started, old_written) =
        environment.configure_respawn_stderr_overlap();
    let session_id = "stderr-respawn-overlap";
    let mut command = environment.command();
    command.args([
        "serve",
        "--session",
        session_id,
        "--create",
        "--mirror-extension-stderr",
    ]);
    let mut server = OwnedProcessGroup::spawn_piped_stderr(&command, environment.temp.path())
        .expect("spawn respawn-overlap server");
    let stderr = server.take_stderr().expect("capture serve stderr");
    let (stderr_line_tx, stderr_line_rx) = mpsc::channel();
    let stderr_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut output = Vec::new();
        loop {
            let mut line = Vec::new();
            let read = reader
                .read_until(b'\n', &mut line)
                .expect("read serve stderr");
            if read == 0 {
                break;
            }
            output.extend_from_slice(&line);
            stderr_line_tx
                .send(line)
                .expect("send captured stderr line");
        }
        output
    });
    let _metadata = environment.wait_for_metadata();
    let first_pid = std::fs::read_to_string(&first_pid_path).expect("read first child PID");
    Command::new("kill")
        .args(["-TERM", first_pid.trim()])
        .status()
        .expect("terminate first extension generation");
    std::fs::write(&release_protocol, b"release").expect("release old protocol pipe");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !second_started.exists() {
        assert!(
            Instant::now() < deadline,
            "replacement generation did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let second_pid = std::fs::read_to_string(&second_started).expect("read second child PID");
    std::fs::write(&release_old, b"release").expect("release old inherited stderr");
    while !old_written.exists() {
        assert!(
            Instant::now() < deadline,
            "old stderr writer did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut saw_new = false;
    loop {
        let line = stderr_line_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("wait for mirrored overlap record");
        let line = String::from_utf8(line).expect("mirror line is UTF-8");
        saw_new |= line.contains("generation=1") && line.contains("message=\"new-start\"");
        if line.contains("generation=0") && line.contains("message=\"old-late\"") {
            break;
        }
    }
    assert!(saw_new, "replacement record must precede late old stderr");
    let status = server.terminate().expect("clean up overlap process group");
    assert!(status.success());
    let stderr =
        String::from_utf8(stderr_reader.join().expect("stderr reader joins")).expect("UTF-8");
    let new = stderr
        .find("generation=1")
        .expect("replacement generation record");
    let old = stderr
        .find("generation=0")
        .and_then(|start| {
            stderr[start..]
                .find("message=\"old-late\"")
                .map(|offset| start + offset)
        })
        .expect("late old-generation record");
    assert!(new < old, "old stderr did not overlap the replacement");
    assert!(stderr.contains(&format!(
        "extension=std-notifications generation=0 pid={} boundary=line message=\"old-late\"",
        first_pid.trim()
    )));
    assert!(stderr.contains(&format!(
        "extension=std-notifications generation=1 pid={} boundary=line message=\"new-start\"",
        second_pid.trim()
    )));
    assert_ne!(first_pid.trim(), second_pid.trim());
    assert!(
        !stderr.contains("stdout-protocol-private"),
        "Configure/protocol canary leaked into process stderr"
    );
}

/// A nested worker exposes the guarded real-respawn fixture to deterministic
/// panic and external-SIGTERM lifecycle tests.
#[test]
fn owned_process_group_cleanup_worker() {
    let Ok(mode) = std::env::var("TAU_OWNED_PROCESS_GROUP_WORKER") else {
        return;
    };
    let environment = TestEnvironment::new();
    let (first_started, ..) = environment.configure_respawn_stderr_overlap();
    let mut command = environment.command();
    command.args([
        "serve",
        "--session",
        "stderr-respawn-overlap-cleanup-worker",
        "--create",
        "--mirror-extension-stderr",
    ]);
    let mut server = OwnedProcessGroup::spawn_piped_stderr(&command, environment.temp.path())
        .expect("spawn guarded cleanup worker server");
    let stderr = server.take_stderr().expect("capture cleanup worker stderr");
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut sink = Vec::new();
        stderr
            .read_to_end(&mut sink)
            .expect("drain cleanup worker stderr");
    });
    let metadata = environment.wait_for_metadata();
    let socket = metadata.with_extension("sock");
    let first_pid =
        std::fs::read_to_string(first_started).expect("read first worker extension PID");
    let identities = wait_for_owned_fixture_members(server.pgid(), first_pid.trim());
    server
        .track_pids(
            identities
                .iter()
                .map(|identity| (identity.pid, identity.start_time)),
        )
        .expect("publish fixture PIDs to cleanup watchdog");
    if mode == "sigterm-during-track-publication" {
        server
            .publish_incomplete_track_frame()
            .expect("publish deliberately incomplete tracking frame");
    }
    let readiness = format!(
        "READY\t{}\t{}\t{}\t{}\t{}\t{}\n",
        server.pgid(),
        process_identity(server.watchdog_pid())
            .expect("watchdog identity")
            .encode(),
        environment.temp.path().display(),
        metadata.display(),
        socket.display(),
        identities
            .iter()
            .map(ProcessIdentity::encode)
            .collect::<Vec<_>>()
            .join(",")
    );
    std::io::stdout()
        .write_all(readiness.as_bytes())
        .expect("publish cleanup worker readiness");
    std::io::stdout()
        .flush()
        .expect("flush cleanup worker readiness");

    match mode.as_str() {
        "panic" => panic!("intentional owned-process-group cleanup panic"),
        "sigterm" | "sigterm-during-track-publication" => {
            let mut release = [0_u8; 1];
            std::io::stdin()
                .read_exact(&mut release)
                .expect("external SIGTERM must terminate the blocked worker");
        }
        "sigterm-after-group-cleanup" => {
            let status = server
                .terminate()
                .expect("explicit worker process-group cleanup");
            assert!(
                status.success(),
                "worker process group exited unsuccessfully"
            );
            std::io::stdout()
                .write_all(b"GROUP_CLEANED\n")
                .expect("publish cleaned-group boundary");
            std::io::stdout()
                .flush()
                .expect("flush cleaned-group boundary");
            let mut release = [0_u8; 1];
            std::io::stdin()
                .read_exact(&mut release)
                .expect("external SIGTERM must terminate the cleanup-boundary worker");
        }
        other => panic!("unknown owned-process-group worker mode: {other}"),
    }
}

/// Hidden entrypoint for the parent-liveness watchdog that remains outside the
/// Tau process group and acts only after its control pipe reaches EOF.
#[test]
fn owned_process_group_watchdog_worker() {
    run_watchdog_worker_from_env();
}

/// A stale or reused numeric PGID whose leader start identity differs from the
/// committed owner is never signaled by the watchdog.
#[test]
fn owned_process_group_watchdog_rejects_mismatched_anchor() {
    let root = tempfile::tempdir().expect("watchdog mismatch temporary root");
    assert!(
        watchdog_rejects_mismatched_anchor(root.path())
            .expect("run mismatched-anchor watchdog canary"),
        "watchdog signaled an identity-mismatched process group"
    );
    assert!(
        !root.path().exists(),
        "watchdog did not remove mismatched-anchor canary root"
    );
}

/// The watchdog observes the exact leader stopped with its committed start
/// identity before it uses the numeric process-group signal.
#[test]
fn owned_process_group_watchdog_stops_matching_anchor_before_group_signal() {
    let root = tempfile::tempdir().expect("watchdog stop-boundary temporary root");
    assert!(
        watchdog_confirms_matching_anchor_stopped(root.path())
            .expect("run stopped-anchor watchdog canary"),
        "watchdog did not confirm the matching leader stopped before group signal"
    );
    assert!(
        !root.path().exists(),
        "watchdog did not remove stopped-anchor canary root"
    );
}

/// A transient non-ready child poll after root-only commit is retried within
/// the existing cleanup deadline instead of becoming an immediate failure.
#[test]
fn owned_process_group_retries_transient_child_reap_poll() {
    assert!(
        child_reap_poll_survives_transient_none().expect("run child reap polling oracle"),
        "bounded child reap did not retry one transient non-ready poll"
    );
}

/// Root-only cleanup revokes numeric-PGID signaling but retains exact tracked
/// identity observation until the process disappears.
#[test]
fn owned_process_group_root_only_waits_for_tracked_identity() {
    let root = tempfile::tempdir().expect("root-only identity-wait temporary root");
    assert!(
        watchdog_waits_for_root_only_tracked_identity(root.path())
            .expect("run root-only identity-wait watchdog canary"),
        "root-only watchdog returned early or signaled the tracked canary"
    );
    assert!(
        !root.path().exists(),
        "watchdog did not remove root-only identity-wait canary root"
    );
}

/// Panic unwinding tears down and reaps the exact real-respawn process group
/// before the worker's temporary environment disappears.
#[test]
fn owned_process_group_cleans_real_respawn_fixture_on_panic() {
    assert_owned_process_group_worker_cleanup("panic");
}

/// External SIGTERM bypasses Rust unwinding but the parent-liveness watchdog
/// still removes only the real-respawn worker's group and temporary resources.
#[test]
fn owned_process_group_cleans_real_respawn_fixture_on_sigterm() {
    assert_owned_process_group_worker_cleanup("sigterm");
}

/// External SIGTERM during an incomplete tracking publication cannot stop the
/// watchdog from killing the exact group and removing its exact resources.
#[test]
fn owned_process_group_cleans_after_interrupted_tracking_publication() {
    assert_owned_process_group_worker_cleanup("sigterm-during-track-publication");
}

/// The watchdog remains armed after explicit group cleanup until owner Drop has
/// removed the exact root, closing the cleanup-order cancellation window.
#[test]
fn owned_process_group_cleans_if_sigterm_follows_group_cleanup() {
    assert_owned_process_group_worker_cleanup("sigterm-after-group-cleanup");
}

/// One PID and Linux start time, used to reject PID reuse in cleanup oracles.
#[derive(Clone, Copy, Debug)]
struct ProcessIdentity {
    /// Linux process identifier.
    pid: u32,
    /// Field 22 from `/proc/<pid>/stat`.
    start_time: u64,
}

impl ProcessIdentity {
    /// Encodes one identity for the worker's readiness record.
    fn encode(&self) -> String {
        format!("{}:{}", self.pid, self.start_time)
    }

    /// Parses one identity from the worker's readiness record.
    fn parse(encoded: &str) -> Self {
        let (pid, start_time) = encoded.split_once(':').expect("encoded process identity");
        Self {
            pid: pid.parse().expect("process identity PID"),
            start_time: start_time.parse().expect("process identity start time"),
        }
    }
}

/// Runs one exact nested worker and proves that all captured processes and
/// resources disappear after the selected failure mode.
fn assert_owned_process_group_worker_cleanup(mode: &str) {
    let mut worker =
        Command::new(std::env::current_exe().expect("current integration test binary"))
            .args([
                "--exact",
                "owned_process_group_cleanup_worker",
                "--nocapture",
            ])
            .env("TAU_OWNED_PROCESS_GROUP_WORKER", mode)
            .env("CARGO_BIN_EXE_tau", env!("CARGO_BIN_EXE_tau"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn exact cleanup worker");
    let stdout = worker.stdout.take().expect("capture cleanup worker stdout");
    let mut worker_stderr = worker.stderr.take().expect("capture cleanup worker stderr");
    let (stderr_tx, stderr_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stderr = Vec::new();
        let result = worker_stderr.read_to_end(&mut stderr);
        let _ = stderr_tx.send((result, stderr));
    });
    let mut stdout = BufReader::new(stdout);
    let mut readiness = String::new();
    loop {
        readiness.clear();
        stdout
            .read_line(&mut readiness)
            .expect("read cleanup worker readiness");
        if readiness.starts_with("READY\t") {
            break;
        }
        if readiness.is_empty() {
            let status = worker.wait().expect("reap unready cleanup worker");
            let (read_result, stderr) = stderr_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("collect unready cleanup worker stderr");
            read_result.expect("read unready cleanup worker stderr");
            panic!(
                "cleanup worker exited before readiness: {status}\n{}",
                String::from_utf8_lossy(&stderr)
            );
        }
    }
    let readiness = parse_cleanup_readiness(&readiness);

    if mode == "sigterm-after-group-cleanup" {
        loop {
            let mut boundary = String::new();
            stdout
                .read_line(&mut boundary)
                .expect("read cleanup-order boundary");
            assert!(
                !boundary.is_empty(),
                "cleanup worker exited before group-cleaned boundary"
            );
            if boundary == "GROUP_CLEANED\n" {
                break;
            }
        }
    }
    if mode.starts_with("sigterm") {
        signal_pid(worker.id(), Signal::TERM).expect("SIGTERM cleanup worker");
    }
    drop(worker.stdin.take());

    let (eof_tx, eof_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut remainder = Vec::new();
        let result = stdout.read_to_end(&mut remainder);
        let _ = eof_tx.send((result, remainder));
    });
    let (read_result, remainder) = eof_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("worker and watchdog close readiness pipe before deadline");
    read_result.expect("read cleanup worker output through EOF");
    let status = worker.wait().expect("reap cleanup worker");
    let (stderr_result, worker_stderr) = stderr_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("collect cleanup worker stderr");
    stderr_result.expect("read cleanup worker stderr");
    assert!(
        !status.success(),
        "{mode} cleanup worker unexpectedly succeeded.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&remainder),
        String::from_utf8_lossy(&worker_stderr)
    );
    if mode.starts_with("sigterm") {
        assert_eq!(status.signal(), Some(Signal::TERM.as_raw()));
    }

    assert!(
        !group_exists(readiness.pgid).expect("query cleaned process group"),
        "owned process group {} survived {mode}",
        readiness.pgid
    );
    for identity in &readiness.identities {
        assert_identity_gone(*identity, mode);
    }
    assert_identity_gone(readiness.watchdog, mode);
    assert!(
        !readiness.metadata.exists(),
        "runtime metadata survived {mode}: {}",
        readiness.metadata.display()
    );
    assert!(
        !readiness.socket.exists(),
        "runtime socket survived {mode}: {}",
        readiness.socket.display()
    );
    assert!(
        !readiness.temp_root.exists(),
        "temporary root survived {mode}: {}",
        readiness.temp_root.display()
    );
    assert_no_proc_reference(&readiness.temp_root, mode);
}

/// Parsed readiness data published after the real fixture and both polling
/// helpers have started.
struct CleanupReadiness {
    /// Dedicated Tau process-group identifier.
    pgid: u32,
    /// External parent-liveness watchdog identity.
    watchdog: ProcessIdentity,
    /// Exact temporary root owned by the fixture.
    temp_root: PathBuf,
    /// Exact runtime metadata path.
    metadata: PathBuf,
    /// Exact runtime socket path.
    socket: PathBuf,
    /// Every process captured in the Tau process group at readiness.
    identities: Vec<ProcessIdentity>,
}

/// Parses the worker's single blocking-pipe readiness record.
fn parse_cleanup_readiness(line: &str) -> CleanupReadiness {
    let mut fields = line.trim_end().split('\t');
    assert_eq!(fields.next(), Some("READY"), "worker readiness prefix");
    let pgid = fields
        .next()
        .expect("readiness PGID")
        .parse()
        .expect("numeric readiness PGID");
    let watchdog = ProcessIdentity::parse(fields.next().expect("readiness watchdog identity"));
    let temp_root = PathBuf::from(fields.next().expect("readiness temporary root"));
    let metadata = PathBuf::from(fields.next().expect("readiness metadata"));
    let socket = PathBuf::from(fields.next().expect("readiness socket"));
    let identities = fields
        .next()
        .expect("readiness process identities")
        .split(',')
        .map(ProcessIdentity::parse)
        .collect();
    assert!(fields.next().is_none(), "unexpected readiness fields");
    CleanupReadiness {
        pgid,
        watchdog,
        temp_root,
        metadata,
        socket,
        identities,
    }
}

/// Waits without elapsed-delay synchronization until the Tau group contains
/// its server, extension, and both first-generation polling helpers.
fn wait_for_owned_fixture_members(pgid: u32, first_extension_pid: &str) -> Vec<ProcessIdentity> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let identities = process_group_identities(pgid);
        let first_extension_pid = first_extension_pid
            .parse::<u32>()
            .expect("first extension PID");
        let helper_count = identities
            .iter()
            .filter(|identity| {
                process_cmdline(identity.pid)
                    .is_some_and(|cmdline| cmdline.contains("respawn-stderr-overlap"))
            })
            .count();
        if identities
            .iter()
            .any(|identity| identity.pid == first_extension_pid)
            && 1 < helper_count
        {
            return identities;
        }
        assert!(
            Instant::now() < deadline,
            "real-respawn fixture members did not become ready"
        );
        std::thread::yield_now();
    }
}

/// Captures every live member of one Linux process group.
fn process_group_identities(pgid: u32) -> Vec<ProcessIdentity> {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(process_stat)
        .filter_map(|(identity, process_group)| (process_group == pgid).then_some(identity))
        .collect()
}

/// Reads one PID's identity and process group from Linux procfs.
fn process_stat(pid: u32) -> Option<(ProcessIdentity, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let fields = after_name.split_ascii_whitespace().collect::<Vec<_>>();
    let process_group = fields.get(2)?.parse().ok()?;
    let start_time = fields.get(19)?.parse().ok()?;
    Some((ProcessIdentity { pid, start_time }, process_group))
}

/// Returns one PID's identity while it still denotes the same live process.
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    process_stat(pid).map(|(identity, _)| identity)
}

/// Reads a process command line for exact fixture-helper identification.
fn process_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&bytes).replace('\0', " "))
}

/// Requires one captured PID to be absent or reused by a different process.
fn assert_identity_gone(identity: ProcessIdentity, mode: &str) {
    assert!(
        process_identity(identity.pid)
            .is_none_or(|current| current.start_time != identity.start_time),
        "captured PID {} survived {mode} with start time {}",
        identity.pid,
        identity.start_time
    );
}

/// Requires no process cwd, root, descriptor, or command line to retain the
/// unique fixture root after cleanup.
fn assert_no_proc_reference(temp_root: &Path, mode: &str) {
    let needle = temp_root.as_os_str().as_encoded_bytes();
    for entry in std::fs::read_dir("/proc")
        .expect("read /proc for residue")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.as_bytes().iter().all(u8::is_ascii_digit))
        })
    {
        let process = entry.path();
        for link in ["cwd", "root"] {
            if std::fs::read_link(process.join(link))
                .ok()
                .is_some_and(|path| {
                    path.as_os_str()
                        .as_encoded_bytes()
                        .windows(needle.len())
                        .any(|window| window == needle)
                })
            {
                panic!(
                    "process {} {link} retained fixture root after {mode}",
                    entry.file_name().to_string_lossy()
                );
            }
        }
        if std::fs::read(process.join("cmdline"))
            .ok()
            .is_some_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
        {
            panic!(
                "process {} command line retained fixture root after {mode}",
                entry.file_name().to_string_lossy()
            );
        }
        if let Ok(descriptors) = std::fs::read_dir(process.join("fd")) {
            for descriptor in descriptors.filter_map(Result::ok) {
                if std::fs::read_link(descriptor.path())
                    .ok()
                    .is_some_and(|path| {
                        path.as_os_str()
                            .as_encoded_bytes()
                            .windows(needle.len())
                            .any(|window| window == needle)
                    })
                {
                    panic!(
                        "process {} descriptor retained fixture root after {mode}",
                        entry.file_name().to_string_lossy()
                    );
                }
            }
        }
    }
}

/// Sends one exact signal to one worker PID.
fn signal_pid(pid: u32, signal: Signal) -> std::io::Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).expect("worker PID fits i32"))
        .expect("worker PID is positive");
    kill_process(pid, signal).map_err(Into::into)
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
