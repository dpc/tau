//! Bounded process-group ownership for S8's headless Boot A.

#![cfg(unix)]

use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::os::unix::process::{CommandExt, ExitStatusExt};
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
        loop {
            status = status.or(child.try_wait()?);
            if status.is_some() && !process_group::exists(self.pgid) {
                break;
            }
            if Instant::now() >= clean_deadline {
                break;
            }
            thread::yield_now();
        }
        let mut forced = false;
        if process_group::exists(self.pgid) {
            forced = true;
            let _ = killpg(self.pgid, Signal::SIGTERM);
            wait_for_group_exit(child, &mut status, self.pgid, Duration::from_secs(1))?;
        }
        if process_group::exists(self.pgid) {
            let _ = killpg(self.pgid, Signal::SIGKILL);
            wait_for_group_exit(child, &mut status, self.pgid, Duration::from_secs(1))?;
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

/// Polls the owned leader while waiting boundedly for its process group to
/// disappear.
fn wait_for_group_exit(
    child: &mut Child,
    status: &mut Option<ExitStatus>,
    pgid: Pid,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if status.is_none() {
            *status = child.try_wait()?;
        }
        if !process_group::exists(pgid) || Instant::now() >= deadline {
            return Ok(());
        }
        thread::yield_now();
    }
}

/// Waits boundedly for a fixture readiness file to appear.
fn wait_for_fixture_ready(marker: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !marker.exists() && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(marker.exists(), "fixture did not publish readiness");
}

/// Owns a fixture process whose failure paths must not rely on the behavior
/// under test for cleanup.
struct FixtureProcess {
    /// Headless child protected by direct bounded safety cleanup.
    process: HeadlessProcess,
}

impl FixtureProcess {
    /// Wraps a spawned fixture process before any fallible setup observation.
    fn new(process: HeadlessProcess) -> Self {
        Self { process }
    }
}

impl Deref for FixtureProcess {
    type Target = HeadlessProcess;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl DerefMut for FixtureProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if self.process.child.is_some() {
            let _ = safety_reap_fixture(&mut self.process);
        }
    }
}

/// Reaps a fixture child directly when a test failure leaves ownership active.
fn safety_reap_fixture(process: &mut HeadlessProcess) -> Result<(), Box<dyn std::error::Error>> {
    let _ = killpg(process.pgid, Signal::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(1);
    let child = process
        .child
        .as_mut()
        .ok_or("fixture child ownership already released")?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("fixture child survived safety cleanup".into());
        }
        thread::yield_now();
    }
    process.child.take();
    process
        .stderr_done
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "fixture stderr reader exceeded EOF deadline")?;
    if let Some(reader) = process.stderr_reader.take() {
        reader
            .join()
            .map_err(|_| "fixture stderr reader panicked")?;
    }
    Ok(())
}

/// Ensures zero-grace cleanup reaps a TERM-exiting leader and retains its
/// distinctive exit status.
#[test]
fn zero_grace_cleanup_retains_term_exit_status() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let marker = tempdir.path().join("leader-ready");
    let stderr_path = tempdir.path().join("stderr.bounded");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(
            "trap 'printf \"term-exit\\n\" >&2; exit 23' TERM; \
             : > \"$1\"; \
             while :; do :; done",
        )
        .arg("sh")
        .arg(&marker);
    let mut process = FixtureProcess::new(
        HeadlessProcess::spawn(
            command,
            tempdir.path().join("absent.sock"),
            stderr_path.clone(),
        )
        .expect("spawn TERM-exiting headless leader"),
    );
    let pgid = process.pgid;
    wait_for_fixture_ready(&marker);
    assert!(
        process
            .child
            .as_mut()
            .expect("owned child")
            .try_wait()
            .expect("poll ready child")
            .is_none(),
        "ready leader exited before cleanup"
    );

    let outcome = process
        .reap(Duration::ZERO)
        .expect("zero-grace TERM cleanup");
    let ReapOutcome::Forced(status) = outcome else {
        panic!("zero-grace cleanup must be forced");
    };
    assert_eq!(status.code(), Some(23), "TERM exit status changed");
    assert!(!process_group::exists(pgid));
    assert!(process.child.is_none(), "child ownership was not released");
    assert!(
        process.stderr_reader.is_none(),
        "stderr reader was not joined"
    );
    assert_eq!(
        std::fs::read_to_string(stderr_path).expect("read stderr artifact"),
        "term-exit\n"
    );
}

/// Ensures zero-grace cleanup continues polling the owned leader through the
/// KILL phase after it ignores TERM.
#[test]
fn zero_grace_cleanup_reaps_term_ignoring_leader_after_kill() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let marker = tempdir.path().join("leader-ready");
    let stderr_path = tempdir.path().join("stderr.bounded");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(
            "trap '' TERM; \
             printf 'kill-ready\\n' >&2; \
             : > \"$1\"; \
             while :; do :; done",
        )
        .arg("sh")
        .arg(&marker);
    let mut process = FixtureProcess::new(
        HeadlessProcess::spawn(
            command,
            tempdir.path().join("absent.sock"),
            stderr_path.clone(),
        )
        .expect("spawn TERM-ignoring headless leader"),
    );
    let pgid = process.pgid;
    wait_for_fixture_ready(&marker);
    assert!(
        process
            .child
            .as_mut()
            .expect("owned child")
            .try_wait()
            .expect("poll ready child")
            .is_none(),
        "ready leader exited before cleanup"
    );

    let outcome = process
        .reap(Duration::ZERO)
        .expect("zero-grace KILL cleanup");
    let ReapOutcome::Forced(status) = outcome else {
        panic!("zero-grace cleanup must be forced");
    };
    assert_eq!(
        status.signal(),
        Some(Signal::SIGKILL as i32),
        "KILL termination status changed"
    );
    assert!(!process_group::exists(pgid));
    assert!(process.child.is_none(), "child ownership was not released");
    assert!(
        process.stderr_reader.is_none(),
        "stderr reader was not joined"
    );
    assert_eq!(
        std::fs::read_to_string(stderr_path).expect("read stderr artifact"),
        "kill-ready\n"
    );
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
