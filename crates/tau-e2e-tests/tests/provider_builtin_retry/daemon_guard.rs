//! Private daemon process-group support for provider-builtin retry acceptance.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tau_e2e_tests::ProviderBuiltinFixture;

const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");
const MAX_STDERR_BYTES: u64 = 256 * 1024;

/// Owns the daemon process group until expected graceful completion or cleanup.
pub(super) struct DaemonGuard {
    /// Group containing the daemon and supervised provider.
    pgid: Pid,
    /// Socket that must disappear on clean shutdown.
    socket: PathBuf,
    /// Captured bounded daemon diagnostic.
    stderr_path: PathBuf,
    /// Exit notification produced by the dedicated child reaper.
    exit_rx: mpsc::Receiver<std::io::Result<ExitStatus>>,
    /// Dedicated child reaper, joined after its notification.
    waiter: Option<JoinHandle<()>>,
    /// Bounded stderr drain, joined only after its EOF notification.
    stderr_worker: Option<JoinHandle<()>>,
    /// Completion notification for the bounded stderr drain.
    stderr_done_rx: mpsc::Receiver<std::io::Result<()>>,
}

impl DaemonGuard {
    /// Starts a private provider-builtin-only harness process group.
    pub(super) fn spawn(
        fixture: &ProviderBuiltinFixture,
        socket: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let stderr_path = fixture.root().join("daemon.stderr");
        let mut command = Command::new(HARNESS_DAEMON);
        command
            .env_clear()
            .env("HOME", fixture.root().join("home"))
            .env("XDG_CONFIG_HOME", fixture.root().join("xdg-config"))
            .env("XDG_STATE_HOME", fixture.root().join("xdg-state"))
            .env("XDG_CACHE_HOME", fixture.root().join("xdg-cache"))
            .env("XDG_RUNTIME_DIR", fixture.root().join("xdg-runtime"))
            .env("LANG", "C.UTF-8")
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .process_group(0)
            .arg(socket)
            .arg(fixture.harness_state_dir())
            .arg(fixture.config_dir())
            .arg(fixture.state_dir())
            .arg("new")
            .arg("--provider-builtin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if fixture.uses_test_dummy() {
            command.arg("--test-dummy");
        }
        if let Some(ca_bundle) = std::env::var_os("TAU_E2E_PROVIDER_CA_BUNDLE") {
            command.env("TAU_PROVIDER_CA_BUNDLE", ca_bundle);
        }
        let mut child = command.spawn()?;
        let pgid = Pid::from_raw(child.id().try_into()?);
        let stderr = child.stderr.take().ok_or("daemon stderr pipe is absent")?;
        let stderr_file = File::create(&stderr_path)?;
        let (stderr_done_tx, stderr_done_rx) = mpsc::sync_channel(1);
        let stderr_worker = std::thread::spawn(move || {
            let _ = stderr_done_tx.send(bounded_drain(stderr, stderr_file));
        });
        let (exit_tx, exit_rx) = mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let _ = exit_tx.send(child.wait());
        });
        Ok(Self {
            pgid,
            socket: socket.to_path_buf(),
            stderr_path,
            exit_rx,
            waiter: Some(waiter),
            stderr_worker: Some(stderr_worker),
            stderr_done_rx,
        })
    }

    /// Returns the daemon's bounded failure diagnostic without exposing fixture
    /// request content.
    pub(super) fn diagnostic(&self) -> Result<String, Box<dyn std::error::Error>> {
        let stderr = std::fs::read_to_string(&self.stderr_path)?;
        Ok(if stderr.is_empty() {
            "daemon stderr is empty".to_owned()
        } else {
            stderr
        })
    }

    /// Reaps a clean daemon and rejects leaked descendants or socket state.
    pub(super) fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let status = self
            .exit_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "provider-builtin daemon exceeded shutdown deadline")??;
        self.join_waiter()?;
        if process_group_exists(self.pgid) {
            self.force_cleanup()?;
            let stderr_result = self.join_stderr();
            if process_group_exists(self.pgid) {
                return Err("provider-builtin process group survived SIGKILL".into());
            }
            stderr_result?;
            return Err("provider-builtin daemon leaked a process-group member".into());
        }
        self.join_stderr()?;
        if self.socket.exists() {
            return Err("provider-builtin daemon socket survived shutdown".into());
        }
        if status.success() {
            Ok(())
        } else {
            let stderr = self.diagnostic()?;
            Err(if stderr == "daemon stderr is empty" {
                format!("provider-builtin daemon exited with {status}").into()
            } else {
                stderr.into()
            })
        }
    }

    /// Joins the child reaper after its exit notification.
    fn join_waiter(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.waiter
            .take()
            .expect("daemon waiter is available")
            .join()
            .map_err(|_| "daemon waiter panicked".into())
    }

    /// Joins the stderr drain only after bounded EOF notification.
    fn join_stderr(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.stderr_done_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "daemon stderr remained open after process exit")??;
        self.stderr_worker
            .take()
            .expect("daemon stderr worker is available")
            .join()
            .map_err(|_| "daemon stderr worker panicked".into())
    }

    /// Terminates the full group and deadline-verifies its disappearance.
    fn force_cleanup(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = killpg(self.pgid, Signal::SIGTERM);
        let _ = killpg(self.pgid, Signal::SIGKILL);
        Ok(())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.waiter.is_none() {
            return;
        }
        let _ = self.force_cleanup();
        if self.exit_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
            let _ = self.join_waiter();
        }
        let _ = self.join_stderr();
        if process_group_exists(self.pgid) {
            eprintln!("provider-builtin retry E2E failed to reap its process group");
        }
    }
}

/// Drains all child diagnostics while retaining only a bounded prefix.
fn bounded_drain(mut source: impl Read, mut destination: impl Write) -> std::io::Result<()> {
    let mut retained = (&mut source).take(MAX_STDERR_BYTES);
    std::io::copy(&mut retained, &mut destination)?;
    std::io::copy(&mut source, &mut std::io::sink())?;
    Ok(())
}

/// Reports whether a process group still has a live member.
fn process_group_exists(pgid: Pid) -> bool {
    killpg(pgid, None).is_ok()
}
