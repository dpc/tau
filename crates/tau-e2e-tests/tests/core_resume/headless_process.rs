//! Bounded process-group ownership for S8's headless Boot A.

#![cfg(unix)]

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use super::process_group;

const MAX_STDERR_BYTES: usize = 256 * 1024;

/// Whether one bounded reap completed naturally or required signal escalation.
enum ReapOutcome {
    /// Parent and descendants exited within the graceful deadline.
    Clean(ExitStatus),
    /// TERM or KILL escalation removed a surviving process-group member.
    Forced(ExitStatus),
}

/// One headless daemon and all of its supervised extensions.
pub(super) struct HeadlessProcess {
    /// Spawned daemon process, also the private process-group leader.
    child: Option<Child>,
    /// Private process group containing the daemon and supervised provider.
    pgid: Pid,
    /// Generation-specific Unix socket that cleanup must remove.
    socket: PathBuf,
    /// Bounded daemon/provider stderr suffix retained for diagnostics.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Continuous bounded stderr artifact path.
    stderr_path: PathBuf,
    /// Reader thread joined only after its bounded EOF acknowledgement.
    stderr_reader: Option<thread::JoinHandle<()>>,
    /// EOF acknowledgement from the stderr reader.
    stderr_done: mpsc::Receiver<()>,
}

impl HeadlessProcess {
    /// Spawns the daemon as a new process-group leader with bounded
    /// diagnostics.
    pub(super) fn spawn(
        mut command: Command,
        socket: PathBuf,
        stderr_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let pgid = Pid::from_raw(i32::try_from(child.id())?);
        let mut pipe = child.stderr.take().ok_or("headless stderr pipe missing")?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let reader_stderr = Arc::clone(&stderr);
        let (done_tx, stderr_done) = mpsc::channel();
        let stderr_reader = thread::spawn(move || {
            let mut chunk = [0_u8; 8 * 1024];
            while let Ok(read) = pipe.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                let Ok(mut suffix) = reader_stderr.lock() else {
                    break;
                };
                suffix.extend_from_slice(&chunk[..read]);
                if suffix.len() > MAX_STDERR_BYTES {
                    let excess = suffix.len() - MAX_STDERR_BYTES;
                    suffix.drain(..excess);
                }
            }
            let _ = done_tx.send(());
        });
        Ok(Self {
            child: Some(child),
            pgid,
            socket,
            stderr,
            stderr_path,
            stderr_reader: Some(stderr_reader),
            stderr_done,
        })
    }

    /// Waits for clean shutdown after the sole UI disconnects and proves the
    /// complete process group, reader, and generation socket disappeared.
    pub(super) fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let status = match self.reap(Duration::from_secs(15))? {
            ReapOutcome::Clean(status) => status,
            ReapOutcome::Forced(status) => {
                return Err(format!(
                    "headless Boot A required forced process-group cleanup; parent exited {status}"
                )
                .into());
            }
        };
        if !status.success() {
            let bytes = self.stderr_bytes()?;
            let diagnostic = String::from_utf8_lossy(&bytes);
            return Err(format!("headless Boot A exited with {status}: {diagnostic}").into());
        }
        Ok(())
    }

    /// Reaps the parent and every same-group descendant without blocking waits.
    fn reap(&mut self, graceful: Duration) -> Result<ReapOutcome, Box<dyn std::error::Error>> {
        let child = self.child.as_mut().ok_or("headless child already reaped")?;
        let clean_deadline = Instant::now() + graceful;
        let mut status = None;
        while Instant::now() < clean_deadline {
            status = status.or(child.try_wait()?);
            if status.is_some() && !process_group::exists(self.pgid) {
                break;
            }
            thread::yield_now();
        }
        let mut forced = false;
        if process_group::exists(self.pgid) {
            forced = true;
            let _ = killpg(self.pgid, Signal::SIGTERM);
            wait_for_group_exit(self.pgid, Duration::from_secs(1));
        }
        if process_group::exists(self.pgid) {
            let _ = killpg(self.pgid, Signal::SIGKILL);
            wait_for_group_exit(self.pgid, Duration::from_secs(1));
        }
        if process_group::exists(self.pgid) {
            return Err("headless Boot A process group survived SIGKILL deadline".into());
        }
        let parent_deadline = Instant::now() + Duration::from_secs(1);
        while status.is_none() && Instant::now() < parent_deadline {
            status = child.try_wait()?;
            thread::yield_now();
        }
        let status = status.ok_or("headless Boot A parent survived process-group cleanup")?;
        let socket_survived = self.socket.exists();
        if socket_survived {
            let _ = std::fs::remove_file(&self.socket);
        }
        if self
            .stderr_done
            .recv_timeout(Duration::from_secs(1))
            .is_err()
        {
            return Err("headless Boot A stderr reader exceeded EOF deadline".into());
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader
                .join()
                .map_err(|_| "headless stderr reader panicked")?;
        }
        std::fs::write(&self.stderr_path, self.stderr_bytes()?)?;
        self.child.take();
        if socket_survived {
            return Err("headless Boot A socket survived process-group cleanup".into());
        }
        Ok(if forced {
            ReapOutcome::Forced(status)
        } else {
            ReapOutcome::Clean(status)
        })
    }

    fn stderr_bytes(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        self.stderr
            .lock()
            .map(|stderr| stderr.clone())
            .map_err(|_| "headless stderr capture poisoned".into())
    }
}

impl Drop for HeadlessProcess {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let _ = self.reap(Duration::ZERO);
        if let Ok(stderr) = self.stderr_bytes() {
            let _ = std::fs::write(&self.stderr_path, stderr);
        }
    }
}

fn wait_for_group_exit(pgid: Pid, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && process_group::exists(pgid) {
        thread::yield_now();
    }
}

/// Ensures an exited leader cannot release ownership while a TERM-ignoring
/// descendant remains in the private process group.
#[test]
fn cleanup_reaps_descendant_after_headless_leader_exits() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let marker = tempdir.path().join("descendant-ready");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(
            "trap '' HUP TERM; \
             (trap '' HUP TERM; : > \"$1\"; while :; do :; done) & \
             exit 0",
        )
        .arg("sh")
        .arg(&marker);
    let mut process = HeadlessProcess::spawn(
        command,
        tempdir.path().join("absent.sock"),
        tempdir.path().join("stderr.bounded"),
    )
    .expect("spawn adversarial headless group");
    let pgid = process.pgid;
    let deadline = Instant::now() + Duration::from_secs(1);
    while !marker.exists() && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(marker.exists(), "descendant did not publish readiness");
    let outcome = process
        .reap(Duration::from_millis(10))
        .expect("bounded descendant cleanup");
    let ReapOutcome::Forced(status) = outcome else {
        panic!("surviving descendant must require forced cleanup");
    };
    assert!(status.success(), "leader exit changed: {status}");
    assert!(!process_group::exists(pgid));
}
