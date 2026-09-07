use std::fs::{File, Permissions};
use std::io::{self, BufRead as _, BufReader, Read as _, Write};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::Builder;
use std::time::{Duration, Instant};

use rustix_openpty::rustix::termios::Winsize;
use rustix_v1::io::Errno;
use rustix_v1::process::{Pid, Signal, kill_process};

#[path = "detach_attach_cli/owned_process_group.rs"]
mod owned_process_group;
#[path = "detach_attach_cli/sigterm_worker_action.rs"]
mod sigterm_worker_action;
#[path = "detach_attach_cli/subreaper_controller.rs"]
mod subreaper_controller;
mod support;

use owned_process_group::{
    OwnedProcessGroup, child_reap_poll_survives_transient_none, group_exists,
    post_watchdog_spawn_failure_reaps_provisional_children, run_watchdog_worker_from_env,
    watchdog_confirms_matching_anchor_stopped, watchdog_rejects_incomplete_initial_identity,
    watchdog_rejects_mismatched_anchor, watchdog_waits_for_root_only_tracked_identity,
};
use sigterm_worker_action::{SigtermCompletionWorker, complete_worker_via_sigterm};
use subreaper_controller::{
    BrokeredCleanupWorker, require_group_absent, require_group_filtered_echild,
    set_isolated_child_subreaper,
};
use support::bounded_runtime_tempdir;

/// Isolated process environment shared by every CLI in one lifecycle test.
struct TestEnvironment {
    /// Scratch root that owns every test artifact.
    temp: tempfile::TempDir,
    /// Short scratch owner required by Unix-domain socket path limits.
    _runtime_temp: tempfile::TempDir,
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
        let runtime_temp = bounded_runtime_tempdir();
        let config_home = temp.path().join("config");
        let state_home = temp.path().join("state");
        let runtime_dir = runtime_temp.path().to_path_buf();
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
            _runtime_temp: runtime_temp,
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

    /// Returns the sole runtime claim path once the harness publishes it.
    fn wait_for_metadata(&self) -> PathBuf {
        let harnesses = self.runtime_dir.join("tau/harnesses/claims");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let metadata = std::fs::read_dir(&harnesses).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "lock")
                            && std::fs::read(path).ok().is_some_and(|bytes| {
                                serde_json::from_slice::<serde_json::Value>(&bytes).is_ok()
                            })
                    })
            });
            if let Some(metadata) = metadata {
                return metadata;
            }
            assert!(Instant::now() < deadline, "harness claim was not published");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Derives the deterministic socket paired with one runtime claim.
    fn socket_for_claim(&self, claim: &Path) -> PathBuf {
        self.runtime_dir
            .join("tau/harnesses/sockets")
            .join(claim.file_stem().expect("claim key"))
            .with_extension("sock")
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
    fn configure_respawn_stderr_overlap(
        &self,
    ) -> (
        PathBuf,
        PathBuf,
        PathBuf,
        PathBuf,
        PathBuf,
        PathBuf,
        PathBuf,
    ) {
        let script = self.temp.path().join("respawn-stderr-overlap");
        let first_started = self.temp.path().join("respawn-first-started");
        let release_protocol = self.temp.path().join("respawn-release-protocol");
        let release_old = self.temp.path().join("respawn-release-old-stderr");
        let second_started = self.temp.path().join("respawn-second-started");
        let old_written = self.temp.path().join("respawn-old-written");
        let second_pid_staged = self.temp.path().join("respawn-second-pid-staged");
        let release_second_pid = self.temp.path().join("respawn-release-second-pid");
        let current_dir = std::env::current_dir().expect("test current directory");
        let mv = std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
            .map(|directory| directory.join("mv"))
            .map(|candidate| {
                if candidate.is_absolute() {
                    candidate
                } else {
                    current_dir.join(candidate)
                }
            })
            .find(|candidate| {
                candidate.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .expect("find absolute mv executable");
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
             pid_tmp=\"$5.pending.$$\"\n\
             printf '%s' \"$$\" > \"$pid_tmp\"\n\
             : > \"$7\"\n\
             while [ ! -e \"$8\" ]; do sleep 0.01; done\n\
             \"$9\" \"$pid_tmp\" \"$5\"\n\
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
            second_pid_staged
                .to_str()
                .expect("UTF-8 marker path")
                .to_owned(),
            release_second_pid
                .to_str()
                .expect("UTF-8 marker path")
                .to_owned(),
            mv.to_str().expect("UTF-8 mv path").to_owned(),
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
            second_pid_staged,
            release_second_pid,
        )
    }

    /// Requires both runtime coordination directories to contain no files.
    fn assert_runtime_discovery_empty(&self) {
        let harnesses = self.runtime_dir.join("tau/harnesses");
        let count = ["claims", "sockets"]
            .into_iter()
            .flat_map(|directory| {
                std::fs::read_dir(harnesses.join(directory))
                    .ok()
                    .into_iter()
                    .flatten()
            })
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
    let (
        first_pid_path,
        release_protocol,
        release_old,
        second_started,
        old_written,
        second_pid_staged,
        release_second_pid,
    ) = environment.configure_respawn_stderr_overlap();
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
    while !second_pid_staged.exists() {
        assert!(
            Instant::now() < deadline,
            "replacement PID publication did not stage"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !second_started.exists(),
        "replacement PID became visible before final atomic publication"
    );
    std::fs::write(&release_second_pid, b"release").expect("release replacement PID publication");
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
    Builder::new()
        .name("owned-process-worker-stderr".into())
        .spawn(move || {
            let mut stderr = stderr;
            let mut sink = Vec::new();
            stderr
                .read_to_end(&mut sink)
                .expect("drain cleanup worker stderr");
        })
        .expect("spawn cleanup worker stderr reader");
    let metadata = environment.wait_for_metadata();
    let socket = environment.socket_for_claim(&metadata);
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

/// Hidden entrypoint that gives lifecycle cleanup one isolated process-global
/// subreaper without changing child ownership in the shared libtest process.
#[test]
fn owned_process_group_cleanup_controller() {
    let real_mode = std::env::var("TAU_OWNED_PROCESS_GROUP_CONTROLLER").ok();
    let controlled = std::env::var_os("TAU_OWNED_PROCESS_GROUP_DIRECT_REAP_CONTROLLER").is_some();
    if real_mode.is_none() && !controlled {
        return;
    }
    set_isolated_child_subreaper().expect("install isolated cleanup subreaper");
    if let Some(mode) = real_mode {
        assert_owned_process_group_worker_cleanup_in_controller(&mode);
        println!("CONTROLLER_COMPLETE");
    } else {
        run_controlled_broker_oracle().expect("run controlled wait-broker oracle");
        println!("DIRECT_REAP_COMPLETE");
    }
}

/// A controlled worker creates a deeper future-adoption member and a real
/// duplex-readiness watcher whose stdout/stderr outlive worker SIGTERM.
#[test]
fn owned_process_group_broker_canary_worker() {
    if std::env::var_os("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_WORKER").is_none() {
        return;
    }
    let current_exe = std::env::current_exe().expect("current integration test binary");
    let (mut holder_control, holder_endpoint) =
        UnixStream::pair().expect("create controlled holder socket");
    let mut holder = Command::new(&current_exe);
    holder
        .args([
            "--exact",
            "owned_process_group_broker_canary_holder",
            "--nocapture",
        ])
        .env("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_HOLDER", "1")
        .stdin(Stdio::from(OwnedFd::from(holder_endpoint)))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let holder = holder.spawn().expect("spawn controlled holder");
    let mut holder_reader =
        BufReader::new(holder_control.try_clone().expect("clone holder control"));
    let mut holder_readiness = String::new();
    holder_reader
        .read_line(&mut holder_readiness)
        .expect("read controlled holder readiness");
    let member = ProcessIdentity::parse(
        holder_readiness
            .trim_end()
            .strip_prefix("MEMBER_READY\t")
            .expect("controlled member readiness prefix"),
    );

    let (mut liveness, watchdog_liveness) =
        UnixStream::pair().expect("create controlled duplex liveness");
    let mut watcher = Command::new(current_exe);
    watcher
        .args([
            "--exact",
            "owned_process_group_broker_canary_watcher",
            "--nocapture",
        ])
        .env(
            "TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_MEMBER",
            member.encode(),
        )
        .stdin(Stdio::from(OwnedFd::from(watchdog_liveness)))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    let watcher = watcher.spawn().expect("spawn controlled watcher");
    let watcher_identity =
        process_identity(watcher.id()).expect("capture controlled watcher identity");
    let mut readiness_reader =
        BufReader::new(liveness.try_clone().expect("clone controlled liveness"));
    let mut readiness = String::new();
    readiness_reader
        .read_line(&mut readiness)
        .expect("read controlled watcher readiness");
    assert_eq!(readiness, "WATCHDOG_READY\n");

    println!(
        "READY\t{}\t{}\t{}",
        member.pid,
        watcher_identity.encode(),
        member.encode()
    );
    std::io::stdout()
        .flush()
        .expect("flush controlled worker readiness");

    let stdin = std::io::stdin();
    let mut controller = BufReader::new(stdin.lock());
    let mut registration = String::new();
    let read = controller
        .read_line(&mut registration)
        .expect("read controlled registration");
    if read == 0 {
        drop(holder_control);
        drop(liveness);
        drop(holder);
        drop(watcher);
        return;
    }
    assert_eq!(registration, "REGISTERED\n");
    holder_control
        .write_all(b"ADOPT\n")
        .expect("arm controlled holder adoption");
    holder_control
        .flush()
        .expect("flush controlled holder adoption");
    liveness
        .write_all(b"ADOPTED\n")
        .expect("arm controlled watcher");
    liveness.flush().expect("flush controlled watcher arm");
    println!("ADOPTION_ARMED");
    std::io::stdout()
        .flush()
        .expect("flush controlled adoption boundary");

    let _holder_control = holder_control;
    let _holder = holder;
    let _watcher = watcher;
    let _liveness = liveness;
    let mut release = [0_u8; 1];
    controller
        .read_exact(&mut release)
        .expect("SIGTERM must terminate controlled broker worker");
}

/// The intermediate holder keeps the target member outside the controller's
/// child set until worker death closes this holder's stdin.
#[test]
fn owned_process_group_broker_canary_holder() {
    if std::env::var_os("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_HOLDER").is_none() {
        return;
    }
    let current_exe = std::env::current_exe().expect("current integration test binary");
    let mut member = Command::new(current_exe);
    member
        .args([
            "--exact",
            "owned_process_group_broker_canary_member",
            "--nocapture",
        ])
        .env("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_MEMBER_PROCESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let member = member
        .spawn()
        .expect("spawn controlled target-group member");
    let mut member = ProvisionalControlledMember::new(member);
    let identity = member
        .capture_identity()
        .expect("capture controlled target-group identity");
    let stdin = std::io::stdin();
    let mut control =
        UnixStream::from(rustix_v1::io::dup(stdin.as_fd()).expect("duplicate holder fd0"));
    control
        .write_all(format!("MEMBER_READY\t{}\n", identity.encode()).as_bytes())
        .expect("publish member readiness on holder socket");
    control.flush().expect("flush member readiness");
    let mut input = BufReader::new(UnixStream::from(
        rustix_v1::io::dup(stdin.as_fd()).expect("duplicate holder input"),
    ));
    let mut command = String::new();
    let read = input
        .read_line(&mut command)
        .expect("read controlled holder command");
    if read == 0 {
        member
            .cleanup()
            .expect("clean provisional controlled member before adoption");
        return;
    }
    assert_eq!(command, "ADOPT\n");
    let mut eof = Vec::new();
    input
        .read_to_end(&mut eof)
        .expect("read worker-liveness EOF after adoption");
    assert!(eof.is_empty(), "holder received bytes after ADOPT");
    member.disarm();
}

/// The target group member remains live until the controlled watcher signals
/// its exact anchored process group.
#[test]
fn owned_process_group_broker_canary_member() {
    if std::env::var_os("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_MEMBER_PROCESS").is_none() {
        return;
    }
    loop {
        std::thread::park();
    }
}

/// The controlled watcher uses the same duplex fd0 readiness and inherited
/// stdout/stderr contract as the real watchdog.
#[test]
fn owned_process_group_broker_canary_watcher() {
    let Ok(member) = std::env::var("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_MEMBER") else {
        return;
    };
    let member = ProcessIdentity::parse(&member);
    let stdin = std::io::stdin();
    let input =
        UnixStream::from(rustix_v1::io::dup(stdin.as_fd()).expect("duplicate controlled fd0"));
    let mut readiness = UnixStream::from(
        rustix_v1::io::dup(stdin.as_fd()).expect("duplicate controlled readiness"),
    );
    readiness
        .write_all(b"WATCHDOG_READY\n")
        .expect("publish controlled duplex readiness");
    readiness
        .flush()
        .expect("flush controlled duplex readiness");
    let mut input = BufReader::new(input);
    let mut arm = String::new();
    let read = input
        .read_line(&mut arm)
        .expect("read controlled watcher arm");
    if read == 0 {
        return;
    }
    assert_eq!(arm, "ADOPTED\n");
    let mut eof = Vec::new();
    input
        .read_to_end(&mut eof)
        .expect("read controlled worker liveness EOF");
    assert!(
        eof.is_empty(),
        "controlled liveness carried unexpected bytes"
    );

    let pgid = Pid::from_raw(i32::try_from(member.pid).expect("controlled PGID fits i32"))
        .expect("positive controlled PGID");
    rustix_v1::process::kill_process_group(pgid, Signal::KILL)
        .expect("kill exact controlled process group");
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_identity(member.pid) == Some(member) {
        assert!(
            Instant::now() < deadline,
            "controlled watcher retained an unreaped target-group member"
        );
        std::thread::yield_now();
    }
    println!("WATCHDOG_POSTAMBLE");
    eprintln!("WATCHDOG_CLEANUP_COMPLETE");
}

/// Owns a controlled member immediately after spawn until exact ADOPT transfers
/// cleanup authority to the isolated controller.
struct ProvisionalControlledMember {
    /// Direct member child retained for unconditional exact kill and reap.
    child: Option<Child>,
    /// PID/start anchor once procfs identity capture succeeds.
    identity: Option<ProcessIdentity>,
}

impl ProvisionalControlledMember {
    /// Installs direct-child ownership before any later fallible operation.
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            identity: None,
        }
    }

    /// Captures and retains the exact member PID/start group anchor.
    fn capture_identity(&mut self) -> io::Result<ProcessIdentity> {
        let pid = self.child.as_ref().expect("member remains owned").id();
        let identity = process_identity(pid)
            .ok_or_else(|| io::Error::other("controlled member identity disappeared"))?;
        self.identity = Some(identity);
        Ok(identity)
    }

    /// Signals the anchored group when possible, then always kills and reaps
    /// the exact direct child before reporting a group-signal error.
    fn cleanup(&mut self) -> io::Result<()> {
        let mut group_error = None;
        if let Some(identity) = self.identity
            && process_identity(identity.pid) == Some(identity)
        {
            let pgid = Pid::from_raw(i32::try_from(identity.pid).expect("member PGID fits i32"))
                .expect("positive member PGID");
            match rustix_v1::process::kill_process_group(pgid, Signal::KILL) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(error) => group_error = Some(io::Error::from(error)),
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(identity) = self.identity
            && group_exists(identity.pid)?
        {
            return Err(io::Error::other(format!(
                "provisional controlled member group {} survived cleanup",
                identity.pid
            )));
        }
        if let Some(error) = group_error {
            return Err(error);
        }
        Ok(())
    }

    /// Releases the still-live child handle only after exact ADOPT.
    fn disarm(&mut self) {
        drop(self.child.take());
    }
}

impl Drop for ProvisionalControlledMember {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// The combined controlled oracle proves future adoption and watchdog stdout
/// lifetime through the same sole-broker and duplex-fd protocol as the real
/// path.
#[test]
fn owned_process_group_wait_broker_covers_future_adoption_and_stdout_lifetime() {
    run_isolated_controller(None).expect("run isolated controlled wait-broker controller");
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

/// A fallible post-watchdog-spawn setup step occurs only after provisional
/// RAII owns both raw children, so injected failure cannot strand either one.
#[test]
fn owned_process_group_post_watchdog_spawn_failure_reaps_provisional_children() {
    let root = tempfile::tempdir().expect("post-watchdog-spawn failure temporary root");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 30"]);
    assert!(
        post_watchdog_spawn_failure_reaps_provisional_children(&command, root.path())
            .expect("run post-watchdog-spawn failure oracle"),
        "provisional owner did not reap both children after post-watchdog-spawn failure"
    );
}

/// EOF in the initial leader frame never grants numeric authority over a live
/// group that happens to use the supplied PGID.
#[test]
fn owned_process_group_rejects_incomplete_initial_identity() {
    let root = tempfile::tempdir().expect("incomplete initial identity temporary root");
    assert!(
        watchdog_rejects_incomplete_initial_identity(root.path())
            .expect("run incomplete initial identity oracle"),
        "incomplete initial identity observed or signaled the numeric process group"
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

/// A missing worker barrier fails at its local deadline, and the oracle's Drop
/// guard still kills and reaps the exact blocked worker and drains stderr.
#[test]
fn owned_process_group_worker_barrier_timeout_cleans_raw_worker() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "IFS= read -r release"]);
    let worker = BoundedCleanupWorker::spawn(command).expect("spawn blocked barrier worker");
    let identity = process_identity(worker.id()).expect("blocked barrier worker identity");
    let error = worker
        .recv_line_until(Instant::now(), "injected barrier")
        .expect_err("missing worker barrier must time out");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(worker);
    assert_identity_gone(identity, "injected worker barrier timeout");
}

/// One PID and Linux start time, used to reject PID reuse in cleanup oracles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// One line or terminal condition from the worker's stdout reader.
enum WorkerLine {
    Line(String),
    Eof,
    Error(io::Error),
}

/// Owns a nested lifecycle worker and both pipe readers across every panic and
/// early return.
struct BoundedCleanupWorker {
    pid: u32,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: mpsc::Receiver<WorkerLine>,
    stderr: Option<mpsc::Receiver<(io::Result<usize>, Vec<u8>)>>,
}

impl BoundedCleanupWorker {
    /// Spawns one worker and installs bounded ownership before reader setup.
    fn spawn(mut command: Command) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let pid = child.id();
        let mut owner = Self {
            pid,
            child: Some(child),
            stdin: None,
            stdout: mpsc::channel().1,
            stderr: None,
        };
        owner.stdin = owner
            .child
            .as_mut()
            .expect("worker remains provisionally owned")
            .stdin
            .take();
        let stdout = owner
            .child
            .as_mut()
            .expect("worker remains provisionally owned")
            .stdout
            .take()
            .expect("capture cleanup worker stdout");
        let stderr = owner
            .child
            .as_mut()
            .expect("worker remains provisionally owned")
            .stderr
            .take()
            .expect("capture cleanup worker stderr");

        let (stdout_tx, stdout_rx) = mpsc::channel();
        Builder::new()
            .name("owned-process-oracle-stdout".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = stdout_tx.send(WorkerLine::Eof);
                            break;
                        }
                        Ok(_) => {
                            if stdout_tx.send(WorkerLine::Line(line)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = stdout_tx.send(WorkerLine::Error(error));
                            break;
                        }
                    }
                }
            })?;
        owner.stdout = stdout_rx;

        let (stderr_tx, stderr_rx) = mpsc::channel();
        Builder::new()
            .name("owned-process-oracle-stderr".into())
            .spawn(move || {
                let mut stderr = stderr;
                let mut bytes = Vec::new();
                let result = stderr.read_to_end(&mut bytes);
                let _ = stderr_tx.send((result, bytes));
            })?;
        owner.stderr = Some(stderr_rx);
        Ok(owner)
    }

    /// Returns the direct worker PID while it remains owned.
    fn id(&self) -> u32 {
        self.pid
    }

    /// Relinquishes only the `Child` wait handle after the sole broker starts;
    /// stdin and both pipe readers remain owned here.
    fn release_child_wait_handle(&mut self) -> Option<Child> {
        self.child.take()
    }

    /// Receives one whole line before the caller's local barrier deadline.
    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.stdout.recv_timeout(remaining) {
            Ok(WorkerLine::Line(line)) => Ok(Some(line)),
            Ok(WorkerLine::Eof) => Ok(None),
            Ok(WorkerLine::Error(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("cleanup worker did not publish {boundary} before deadline"),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("cleanup worker {boundary} reader disconnected"),
            )),
        }
    }

    /// Closes the worker release pipe.
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// Writes one explicit controlled-test barrier without relinquishing the
    /// retained stdin failure authority.
    fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker stdin is closed"))?;
        stdin.write_all(bytes)?;
        stdin.flush()
    }

    /// Waits for the direct worker only within the caller's deadline.
    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        let child = self.child.as_mut().expect("worker remains owned");
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok(Some(status));
            }
            if deadline <= Instant::now() {
                return Ok(None);
            }
            std::thread::yield_now();
        }
    }

    /// Collects the complete stderr stream before the local deadline.
    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        let receiver = self
            .stderr
            .as_ref()
            .expect("cleanup worker stderr remains owned");
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (result, stderr) = receiver.recv_timeout(remaining).map_err(|error| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("cleanup worker stderr did not close before deadline: {error}"),
            )
        })?;
        self.stderr.take();
        result?;
        Ok(stderr)
    }

    /// Kills and reaps the exact worker, then gives both readers a bounded
    /// opportunity to observe pipe closure.
    fn cleanup(&mut self) {
        self.close_stdin();
        if let Some(child) = self.child.as_mut() {
            let _ = signal_pid(child.id(), Signal::KILL);
            let deadline = Instant::now() + Duration::from_secs(2);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::yield_now();
            }
        }
        self.child.take();
        let reader_deadline = Instant::now() + Duration::from_secs(2);
        while self
            .recv_line_until(reader_deadline, "stdout EOF")
            .ok()
            .flatten()
            .is_some()
        {}
        if self.stderr.is_some() {
            let _ = self.collect_stderr_until(reader_deadline);
        }
    }
}

impl Drop for BoundedCleanupWorker {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl SigtermCompletionWorker for BoundedCleanupWorker {
    fn id(&self) -> u32 {
        self.id()
    }

    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>> {
        self.recv_line_until(deadline, boundary)
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        self.wait_until(deadline)
    }

    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        self.collect_stderr_until(deadline)
    }
}

/// Runs one exact nested worker and proves that all captured processes and
/// resources disappear after the selected failure mode.
fn assert_owned_process_group_worker_cleanup(mode: &str) {
    if mode.starts_with("sigterm") {
        run_isolated_controller(Some(mode)).expect("run isolated cleanup controller");
        return;
    }
    assert_owned_process_group_panic_cleanup(mode);
}

/// Runs one hidden controller with the existing lifecycle deadline and retains
/// all controller stdout/stderr on failure.
fn run_isolated_controller(mode: Option<&str>) -> io::Result<()> {
    let mut command =
        Command::new(std::env::current_exe().expect("current integration test binary"));
    command.args([
        "--exact",
        "owned_process_group_cleanup_controller",
        "--nocapture",
    ]);
    if let Some(mode) = mode {
        command
            .env("TAU_OWNED_PROCESS_GROUP_CONTROLLER", mode)
            .env("CARGO_BIN_EXE_tau", env!("CARGO_BIN_EXE_tau"));
    } else {
        command.env("TAU_OWNED_PROCESS_GROUP_DIRECT_REAP_CONTROLLER", "1");
    }
    let mut controller = BoundedCleanupWorker::spawn(command)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stdout = String::new();
    while let Some(line) = controller.recv_line_until(deadline, "controller stdout EOF")? {
        stdout.push_str(&line);
    }
    let status = controller
        .wait_until(deadline)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "reap cleanup controller"))?;
    let stderr = controller.collect_stderr_until(deadline)?;
    let expected = if mode.is_some() {
        "CONTROLLER_COMPLETE\n"
    } else {
        "DIRECT_REAP_COMPLETE\n"
    };
    if !status.success() || !stdout.contains(expected) {
        return Err(io::Error::other(format!(
            "isolated cleanup controller failed: {status}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    Ok(())
}

/// Retains the original panic/unwind oracle outside the SIGTERM subreaper
/// protocol.
fn assert_owned_process_group_panic_cleanup(mode: &str) {
    let mut command =
        Command::new(std::env::current_exe().expect("current integration test binary"));
    command
        .args([
            "--exact",
            "owned_process_group_cleanup_worker",
            "--nocapture",
        ])
        .env("TAU_OWNED_PROCESS_GROUP_WORKER", mode)
        .env("CARGO_BIN_EXE_tau", env!("CARGO_BIN_EXE_tau"));
    let mut worker = BoundedCleanupWorker::spawn(command).expect("spawn exact cleanup worker");
    let readiness_deadline = Instant::now() + Duration::from_secs(10);
    let readiness = loop {
        let Some(line) = worker
            .recv_line_until(readiness_deadline, "READY")
            .expect("read cleanup worker readiness before deadline")
        else {
            let stderr = worker
                .collect_stderr_until(readiness_deadline)
                .expect("collect unready cleanup worker stderr");
            panic!(
                "cleanup worker exited before readiness\n{}",
                String::from_utf8_lossy(&stderr)
            );
        };
        if line.starts_with("READY\t") {
            break line;
        }
    };
    let readiness = parse_cleanup_readiness(&readiness);
    let completion_deadline = Instant::now() + Duration::from_secs(10);
    let mut remainder = String::new();
    while let Some(line) = worker
        .recv_line_until(completion_deadline, "stdout EOF")
        .expect("worker and watchdog close stdout before deadline")
    {
        remainder.push_str(&line);
    }
    let status = worker
        .wait_until(completion_deadline)
        .expect("poll cleanup worker status")
        .expect("reap cleanup worker before deadline");
    let worker_stderr = worker
        .collect_stderr_until(completion_deadline)
        .expect("collect cleanup worker stderr before deadline");
    assert!(
        !status.success(),
        "{mode} cleanup worker unexpectedly succeeded.\nstdout:\n{}\nstderr:\n{}",
        remainder,
        String::from_utf8_lossy(&worker_stderr)
    );
    assert!(!mode.starts_with("sigterm"));

    assert_cleanup_readiness_absent(&readiness, mode);
}

/// Executes the SIGTERM lifecycle after the isolated controller installs its
/// sole wait broker.
fn assert_owned_process_group_worker_cleanup_in_controller(mode: &str) {
    assert!(mode.starts_with("sigterm"), "controller owns SIGTERM modes");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut command =
        Command::new(std::env::current_exe().expect("current integration test binary"));
    command
        .args([
            "--exact",
            "owned_process_group_cleanup_worker",
            "--nocapture",
        ])
        .env("TAU_OWNED_PROCESS_GROUP_WORKER", mode)
        .env("CARGO_BIN_EXE_tau", env!("CARGO_BIN_EXE_tau"));
    let mut worker =
        BrokeredCleanupWorker::spawn(command, deadline).expect("spawn brokered cleanup worker");
    let readiness = loop {
        let Some(line) = worker
            .recv_line_until(deadline, "READY")
            .expect("read cleanup worker readiness before deadline")
        else {
            panic!("cleanup worker exited before readiness");
        };
        if line.starts_with("READY\t") {
            break parse_cleanup_readiness(&line);
        }
    };
    let group_anchor = readiness
        .identities
        .iter()
        .copied()
        .find(|identity| identity.pid == readiness.pgid)
        .expect("readiness retains exact Tau group leader");
    worker
        .register_readiness(readiness.watchdog, group_anchor)
        .expect("register exact broker sentinels");

    if mode == "sigterm-after-group-cleanup" {
        loop {
            let boundary = worker
                .recv_line_until(deadline, "GROUP_CLEANED")
                .expect("read cleanup-order boundary before deadline")
                .expect("cleanup worker exited before group-cleaned boundary");
            if boundary == "GROUP_CLEANED\n" {
                break;
            }
        }
    }
    let completion = complete_worker_via_sigterm(&mut worker, deadline, |_, _| Ok(()))
        .expect("complete brokered cleanup worker through SIGTERM");
    worker
        .finish_broker(&completion.stderr)
        .expect("retain exact worker/watchdog statuses and final ECHILD");
    assert_eq!(completion.status.signal(), Some(Signal::TERM.as_raw()));
    assert_cleanup_readiness_absent(&readiness, mode);
}

/// Runs the combined future-adoption and inherited-stdout canary through the
/// same broker and SIGTERM action as the real cleanup oracle.
fn run_controlled_broker_oracle() -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    run_controlled_prearm_abort(deadline)?;
    run_controlled_broker_success(deadline)
}

/// Aborts before registration so the holder must kill and reap its exact
/// member before controller ownership can transfer.
fn run_controlled_prearm_abort(deadline: Instant) -> io::Result<()> {
    let (mut worker, pgid, watchdog, member) = spawn_controlled_broker_worker(deadline)?;
    require_group_filtered_echild(pgid)?;
    worker.close_stdin();
    let _stdout = drain_controlled_stdout(&worker, deadline)?;
    let status = worker
        .wait_until(deadline)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "aborted worker status"))?;
    let stderr = worker.collect_stderr_until(deadline)?;
    worker.finish_prearm_abort(watchdog, &stderr)?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "pre-arm worker failed: {status}\nstderr:\n{}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    require_group_absent(pgid)?;
    if process_identity(member.pid).is_some() || process_identity(watchdog.pid).is_some() {
        return Err(io::Error::other(
            "pre-arm member or watcher survived provisional cleanup",
        ));
    }
    Ok(())
}

/// Executes the strict REGISTERED-to-ADOPT ownership transfer before the sole
/// SIGTERM action and final broker completion.
fn run_controlled_broker_success(deadline: Instant) -> io::Result<()> {
    let (mut worker, pgid, watchdog, member) = spawn_controlled_broker_worker(deadline)?;
    require_group_filtered_echild(pgid)?;
    worker.register_readiness(watchdog, member)?;
    worker.write_stdin(b"REGISTERED\n")?;
    loop {
        let line = worker
            .recv_line_until(deadline, "ADOPTION_ARMED")?
            .ok_or_else(|| io::Error::other("controlled worker exited before adoption arm"))?;
        if line == "ADOPTION_ARMED\n" {
            break;
        }
    }
    let completion = complete_worker_via_sigterm(&mut worker, deadline, |_, _| Ok(()))?;
    worker.finish_broker(&completion.stderr)?;
    if completion.status.signal() != Some(Signal::TERM.as_raw()) {
        return Err(io::Error::other(format!(
            "controlled worker terminal was not SIGTERM: {}",
            completion.status
        )));
    }
    if !completion.stdout.contains("WATCHDOG_POSTAMBLE\n") {
        return Err(io::Error::other(format!(
            "controlled watcher stdout postamble missing before EOF:\n{}",
            completion.stdout
        )));
    }
    if !completion
        .stderr
        .windows(b"WATCHDOG_CLEANUP_COMPLETE\n".len())
        .any(|window| window == b"WATCHDOG_CLEANUP_COMPLETE\n")
    {
        return Err(io::Error::other(format!(
            "controlled watcher stderr completion missing before EOF:\n{}",
            String::from_utf8_lossy(&completion.stderr)
        )));
    }
    require_group_absent(pgid)?;
    if process_identity(member.pid).is_some() || process_identity(watchdog.pid).is_some() {
        return Err(io::Error::other(
            "controlled member or watcher survived broker completion",
        ));
    }
    Ok(())
}

/// Spawns one controlled worker and parses its socket-sourced member plus
/// duplex-watcher readiness record.
fn spawn_controlled_broker_worker(
    deadline: Instant,
) -> io::Result<(BrokeredCleanupWorker, u32, ProcessIdentity, ProcessIdentity)> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "owned_process_group_broker_canary_worker",
            "--nocapture",
        ])
        .env("TAU_OWNED_PROCESS_GROUP_BROKER_CANARY_WORKER", "1");
    let worker = BrokeredCleanupWorker::spawn(command, deadline)?;
    let readiness = loop {
        let line = worker
            .recv_line_until(deadline, "controlled READY")?
            .ok_or_else(|| io::Error::other("controlled worker exited before READY"))?;
        if line.starts_with("READY\t") {
            break line;
        }
    };
    let mut fields = readiness.trim_end().split('\t');
    if fields.next() != Some("READY") {
        return Err(io::Error::other("controlled readiness prefix"));
    }
    let pgid = fields
        .next()
        .ok_or_else(|| io::Error::other("controlled readiness PGID"))?
        .parse::<u32>()
        .map_err(io::Error::other)?;
    let watchdog = ProcessIdentity::parse(
        fields
            .next()
            .ok_or_else(|| io::Error::other("controlled watcher identity"))?,
    );
    let member = ProcessIdentity::parse(
        fields
            .next()
            .ok_or_else(|| io::Error::other("controlled member identity"))?,
    );
    if fields.next().is_some() || member.pid != pgid {
        return Err(io::Error::other("invalid controlled readiness fields"));
    }
    Ok((worker, pgid, watchdog, member))
}

/// Drains controlled stdout through holder, worker, and watcher terminal EOF.
fn drain_controlled_stdout(
    worker: &BrokeredCleanupWorker,
    deadline: Instant,
) -> io::Result<String> {
    let mut stdout = String::new();
    while let Some(line) = worker.recv_line_until(deadline, "controlled stdout EOF")? {
        stdout.push_str(&line);
    }
    Ok(stdout)
}

/// Retains every original role-specific process and resource assertion.
fn assert_cleanup_readiness_absent(readiness: &CleanupReadiness, mode: &str) {
    assert!(
        !group_exists(readiness.pgid).expect("query cleaned process group"),
        "owned process group {} survived {mode}",
        readiness.pgid
    );
    for (index, identity) in readiness.identities.iter().enumerate() {
        assert_identity_gone(*identity, &format!("{mode} fixture[{index}]"));
    }
    assert_identity_gone(readiness.watchdog, &format!("{mode} watchdog"));
    // A forced cut may leave unlocked claim/socket pathnames. Runtime routing
    // treats those as crash residue; the next lock winner reclaims the socket.
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
    _metadata: PathBuf,
    /// Exact runtime socket path.
    _socket: PathBuf,
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
        _metadata: metadata,
        _socket: socket,
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

    /// Require exactly one final status after terminal cleanup, rather than a
    /// status rendered inside the alternate screen or followed by cleanup
    /// bytes.
    fn assert_final_session_status(&mut self, expected: &str) {
        self.wait_for_text(expected);
        while let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(50)) {
            self.accumulated_output.extend(bytes);
        }
        let output = String::from_utf8_lossy(&self.accumulated_output);
        assert_eq!(
            output.matches("Session detached").count()
                + output.matches("Session terminated").count(),
            1,
            "{output}"
        );
        // Terminal feature-reset controls may precede the stderr line without
        // moving the cursor. Nothing, including cleanup controls, may follow
        // it.
        assert!(output.trim_end().ends_with(expected), "{output}");
        assert!(!output.contains("Session exit unconfirmed"), "{output}");
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
    fn wait_success(&mut self, state_home: &Path) {
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
    environment.wait_for_ready_uis(1);
    let metadata_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata).expect("read metadata"))
            .expect("parse metadata");
    let session_id = metadata_value["session_id"]
        .as_str()
        .expect("metadata session id")
        .to_owned();
    let socket = environment.socket_for_claim(&metadata);
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
    owner.assert_final_session_status("Session detached");
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
        attached.line([":quit", ":q", ":detach"][cycle]);
        attached.wait_success(&environment.state_home);
        attached.assert_final_session_status("Session detached");
        assert!(metadata.exists(), "reattach cycle replaced daemon metadata");
        assert!(socket.exists(), "reattach cycle removed daemon socket");
    }
    let _ = stop_reader_tx.send(());
    metadata_reader.join().expect("concurrent metadata reader");
    let mut shutdown = environment.command();
    shutdown.args(["attach", &session_id]);
    let mut shutdown = PtyChild::spawn(shutdown);
    environment.wait_for_ready_uis(5);
    shutdown.line(":quit-session");
    shutdown.wait_success(&environment.state_home);
    shutdown.assert_final_session_status("Session terminated");
    environment.wait_for_runtime_pair_gone(&metadata, &socket);
}

/// Discovery and exact routing use shared runtime-directory inodes rather than
/// process visibility. A client that is PID 1 in a private PID/proc namespace
/// can still list a daemon running in the parent namespace.
#[cfg(target_os = "linux")]
#[test]
fn session_discovery_crosses_pid_and_proc_namespaces() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let claim = environment.wait_for_metadata();
    environment.wait_for_ready_uis(1);
    let claim_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&claim).expect("read runtime claim"))
            .expect("parse runtime claim");
    let session_id = claim_value["session_id"]
        .as_str()
        .expect("claim session id")
        .to_owned();
    let socket = environment.socket_for_claim(&claim);

    owner.line(":detach");
    owner.wait_success(&environment.state_home);

    let tau = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let output = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
            "sh",
            "-c",
            "test \"$$\" -eq 1 && exec \"$@\"",
            "sh",
            &tau,
            "session",
            "list",
            "--json",
        ])
        .env_clear()
        .env("HOME", environment.temp.path().join("home"))
        .env("XDG_CONFIG_HOME", &environment.config_home)
        .env("XDG_STATE_HOME", &environment.state_home)
        .env("XDG_RUNTIME_DIR", &environment.runtime_dir)
        .env("LANG", "C.UTF-8")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("run namespaced session discovery");
    assert!(
        output.status.success(),
        "namespaced discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("namespaced session list JSON");
    assert!(
        sessions
            .as_array()
            .expect("session list array")
            .iter()
            .any(|session| session["session_id"] == session_id),
        "namespaced discovery omitted fixed session: {sessions}"
    );

    let mut shutdown = environment.command();
    shutdown.args(["attach", &session_id]);
    let mut shutdown = PtyChild::spawn(shutdown);
    environment.wait_for_ready_uis(2);
    shutdown.line(":quit-session");
    shutdown.wait_success(&environment.state_home);
    environment.wait_for_runtime_pair_gone(&claim, &socket);
}

/// Ctrl-D preserves the immediate-UI launch policy and must therefore stop the
/// daemon rather than silently leaving a background session behind.
#[test]
fn owned_cli_eof_stops_daemon_and_removes_discovery_pair() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let claim = environment.wait_for_metadata();
    environment.wait_for_ready_uis(1);
    let socket = environment.socket_for_claim(&claim);

    owner
        .controller
        .write_all(&[4])
        .expect("write terminal EOF");
    owner.controller.flush().expect("flush terminal EOF");
    owner.wait_success(&environment.state_home);
    owner.assert_final_session_status("Session terminated");
    environment.wait_for_runtime_pair_gone(&claim, &socket);
}

/// Both spellings of normal quit must make plain `tau` behave like a foreground
/// program and report confirmed termination after restoring the terminal.
#[test]
fn owned_cli_quit_and_q_stop_the_session() {
    for command in [":quit", ":q"] {
        let environment = TestEnvironment::new();
        let mut owner = PtyChild::spawn(environment.command());
        let claim = environment.wait_for_metadata();
        let socket = environment.socket_for_claim(&claim);
        environment.wait_for_ready_uis(1);
        owner.line(command);
        owner.wait_success(&environment.state_home);
        owner.assert_final_session_status("Session terminated");
        environment.wait_for_runtime_pair_gone(&claim, &socket);
    }
}

/// The policy follows the last UI, not creator ownership. The attached final
/// quitter must confirm the original daemon's death without owning its Child.
#[test]
fn creator_quit_survives_until_last_attached_ui_quits() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let claim = environment.wait_for_metadata();
    let socket = environment.socket_for_claim(&claim);
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&claim).expect("claim")).expect("claim JSON");
    let session_id = value["session_id"].as_str().expect("session");
    environment.wait_for_ready_uis(1);
    let mut command = environment.command();
    command.args(["attach", session_id]);
    let mut attached = PtyChild::spawn(command);
    environment.wait_for_ready_uis(2);
    owner.line(":quit");
    owner.wait_success(&environment.state_home);
    owner.assert_final_session_status("Session detached");
    assert!(claim.exists());
    attached.line(":q");
    attached.wait_success(&environment.state_home);
    attached.assert_final_session_status("Session terminated");
    environment.wait_for_runtime_pair_gone(&claim, &socket);
}

/// Abrupt creator loss cannot rely on a graceful quit request. The daemon must
/// apply its final-UI EOF policy and clean discovery on its own.
#[test]
fn unexpected_initial_ui_process_loss_stops_session() {
    let environment = TestEnvironment::new();
    let mut owner = PtyChild::spawn(environment.command());
    let claim = environment.wait_for_metadata();
    let socket = environment.socket_for_claim(&claim);
    environment.wait_for_ready_uis(1);
    owner.child.kill().expect("kill isolated test UI");
    owner.child.wait().expect("reap isolated test UI");
    environment.wait_for_runtime_pair_gone(&claim, &socket);
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
    let socket = environment.socket_for_claim(&metadata);
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
    attach.wait_for_text("start another Tau invocation in a new terminal");
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
    let resumed_socket = environment.socket_for_claim(&resumed_metadata);
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

/// Managed create-or-existing serve admits one bootstrap generation on first
/// creation, resumes it without touching a missing source, and creates a new
/// agent only when the operator supplies a different bootstrap id.
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
            "--create-or-existing",
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
    let first_socket = environment.socket_for_claim(&first_metadata);
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
            "--create-or-existing",
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
    let restart_socket = environment.socket_for_claim(&restart_metadata);
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
            "--create-or-existing",
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
    let next_socket = environment.socket_for_claim(&next_metadata);
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
    let socket = environment.socket_for_claim(&metadata);
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
    let socket = environment.socket_for_claim(&metadata);
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
    let output = locked
        .command()
        .args(["serve", "--session", "locked", "--create-or-existing"])
        .output()
        .expect("reject create-or-existing over locked session");
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
    let output = malformed
        .command()
        .args(["serve", "--session", "malformed", "--create-or-existing"])
        .output()
        .expect("reject create-or-existing malformed server");
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
    let output = environment
        .command()
        .args(["serve", "--session", session_id, "--create-or-existing"])
        .output()
        .expect("reject create-or-existing partial session");
    assert!(
        !output.status.success(),
        "{session_id} unexpectedly resumed partial state"
    );
    assert_eq!(
        std::fs::read(&artifact).expect("preserved partial artifact"),
        bytes,
        "{session_id} mutated partial state while resuming"
    );
    environment.assert_runtime_discovery_empty();
}
