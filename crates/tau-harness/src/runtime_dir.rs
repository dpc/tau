//! Session-keyed daemon runtime claims and socket discovery.
//!
//! One daemon incarnation owns one lifetime `flock` under
//! `<runtime>/tau/harnesses/claims/` and one deterministic Unix socket under
//! `<runtime>/tau/harnesses/sockets/`. The validated session identity, never a
//! PID or process-generation token, is the routing authority.

#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

const HARNESSES_DIR: &str = "harnesses";
const CLAIMS_DIR: &str = "claims";
const SOCKETS_DIR: &str = "sockets";
const CLAIM_EXTENSION: &str = "lock";
const SOCKET_EXTENSION: &str = "sock";
const CLAIM_VERSION: u32 = 0;
const MAX_CLAIM_BYTES: u64 = 16 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_DISCOVERY_CALLS: usize = 8;
const MAX_DISCOVERY_PROBES: usize = 8;

/// Maximum number of sessions returned by one peer-discovery request.
pub const SESSION_DISCOVERY_MAX_RESULTS: usize = 50;

static ACTIVE_DISCOVERY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE_DISCOVERY_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_DISCOVERY_SCAN_DELAY: LazyLock<Mutex<Option<(PathBuf, u64)>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static TEST_CANCEL_AFTER_CLAIM_READ: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_DISCOVERY_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// One authoritative running session reported by its admitted daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningSession {
    /// Immutable session identity owned by the daemon.
    pub session_id: tau_proto::SessionId,
    /// Canonical project root captured at daemon startup.
    pub project_root: PathBuf,
}

/// Non-queued admission for one top-level peer-discovery call.
#[derive(Clone)]
pub(crate) struct DiscoveryCallPermit {
    /// Shared lease retains the charged slot through the call.
    _lease: Arc<DiscoveryCallLease>,
}

struct DiscoveryCallLease;

impl DiscoveryCallPermit {
    /// Acquires one process-wide discovery slot or rejects immediately.
    pub(crate) fn try_acquire() -> Option<Self> {
        #[allow(deprecated, reason = "workspace MSRV predates AtomicUsize::try_update")]
        ACTIVE_DISCOVERY_CALLS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_DISCOVERY_CALLS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self {
                _lease: Arc::new(DiscoveryCallLease),
            })
    }
}

impl Drop for DiscoveryCallLease {
    fn drop(&mut self) {
        ACTIVE_DISCOVERY_CALLS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One redacted live peer session returned by bounded discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerSession {
    /// Exact session routing key.
    pub session_id: String,
    /// Basename-only project label.
    pub project_label: Option<String>,
    /// Whether this is the caller's own session.
    pub current: bool,
}

/// Complete bounded peer-session snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerSessionSnapshot {
    /// Deterministically session-id-sorted matching sessions.
    pub sessions: Vec<PeerSession>,
    /// Whether the caller's result limit omitted additional matches.
    pub truncated: bool,
    /// Whether discovery could not prove a complete runtime snapshot.
    pub scan_truncated: bool,
}

/// Error returned when exact session resolution cannot determine availability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FindHarnessForSessionError {
    /// The claim is contended but its exact responder cannot be admitted.
    Incomplete {
        /// Requested immutable session identity.
        session_id: String,
    },
}

impl std::fmt::Display for FindHarnessForSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete { session_id } => write!(
                formatter,
                "runtime claim for session `{session_id}` is owned but not responding exactly"
            ),
        }
    }
}

impl std::error::Error for FindHarnessForSessionError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClaimRecord {
    /// Runtime-claim schema version.
    version: u32,
    /// Exact session identity protected by the held lock.
    session_id: tau_proto::SessionId,
    /// Canonical startup project root.
    project_root: PathBuf,
    /// Whether the daemon accepts inter-harness peer traffic.
    peer_entrypoint: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    /// Filesystem device number.
    device: u64,
    /// Filesystem inode number.
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Lifetime runtime ownership for one immutable session daemon.
pub struct SessionClaim {
    /// Held exclusive lock; this field drops last.
    file: File,
    /// Device/inode identity of the locked claim.
    identity: FileIdentity,
    /// Exact claim pathname.
    claim_path: PathBuf,
    /// Deterministic socket pathname.
    socket_path: PathBuf,
    /// Diagnostic record written only while the lock is held.
    record: ClaimRecord,
}

impl SessionClaim {
    /// Returns the deterministic socket pathname protected by this claim.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Publishes bounded diagnostics after the exact responder is ready.
    pub fn publish(&mut self, peer_entrypoint: bool) -> io::Result<()> {
        self.record.peer_entrypoint = peer_entrypoint;
        let bytes = serde_json::to_vec(&self.record).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_CLAIM_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime claim record is too large",
            ));
        }
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.flush()
    }

    /// Reclaims only the exact stale socket while holding the session lock.
    pub fn reclaim_stale_socket(&self) -> io::Result<()> {
        let metadata = match std::fs::symlink_metadata(&self.socket_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != current_euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to replace non-owned socket path `{}`",
                    self.socket_path.display()
                ),
            ));
        }
        std::fs::remove_file(&self.socket_path)
    }

    /// Retires the claim pathname while the lock is still held.
    pub fn retire(self) -> io::Result<()> {
        self.remove_owned_claim_path()
    }

    fn remove_owned_claim_path(&self) -> io::Result<()> {
        let metadata = match std::fs::symlink_metadata(&self.claim_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if FileIdentity::from_metadata(&metadata) == self.identity {
            std::fs::remove_file(&self.claim_path)?;
        }
        Ok(())
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        let _ = self.remove_owned_claim_path();
    }
}

/// Returns the effective Tau runtime root for the calling thread.
#[must_use]
pub fn root_runtime_dir() -> PathBuf {
    if let Some(dir) = runtime_dir_override() {
        return dir.join("tau");
    }
    dirs::runtime_dir()
        .map(|dir| dir.join("tau"))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/tau-{}", current_euid())))
}

fn runtime_dir_override() -> Option<PathBuf> {
    RUNTIME_DIR_OVERRIDE.with(|dir| dir.borrow().clone())
}

/// Restores the calling thread's previous runtime-directory override.
pub(crate) struct RuntimeDirOverride {
    /// Previous override restored on drop.
    previous: Option<PathBuf>,
}

impl Drop for RuntimeDirOverride {
    fn drop(&mut self) {
        RUNTIME_DIR_OVERRIDE.with(|dir| *dir.borrow_mut() = self.previous.take());
    }
}

fn override_runtime_dir(path: &Path) -> RuntimeDirOverride {
    let previous = RUNTIME_DIR_OVERRIDE.with(|dir| dir.borrow_mut().replace(path.to_path_buf()));
    RuntimeDirOverride { previous }
}

/// Runs one embedded operation with an explicit thread-scoped runtime parent.
pub(crate) fn with_runtime_dir<T>(path: Option<&Path>, operation: impl FnOnce() -> T) -> T {
    let Some(path) = path else {
        return operation();
    };
    let _scope = override_runtime_dir(path);
    operation()
}

thread_local! {
    static RUNTIME_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Returns the common private harness runtime directory.
#[must_use]
pub fn harnesses_dir() -> PathBuf {
    root_runtime_dir().join(HARNESSES_DIR)
}

fn claims_dir() -> PathBuf {
    harnesses_dir().join(CLAIMS_DIR)
}

fn sockets_dir() -> PathBuf {
    harnesses_dir().join(SOCKETS_DIR)
}

/// Creates and validates the private runtime directory hierarchy.
pub(crate) fn prepare_harnesses_dir() -> io::Result<PathBuf> {
    let root = root_runtime_dir();
    ensure_private_runtime_dir(&root)?;
    let harnesses = root.join(HARNESSES_DIR);
    ensure_private_runtime_dir(&harnesses)?;
    ensure_private_runtime_dir(&harnesses.join(CLAIMS_DIR))?;
    ensure_private_runtime_dir(&harnesses.join(SOCKETS_DIR))?;
    std::fs::canonicalize(harnesses)
}

fn session_key(session_id: &tau_proto::SessionId) -> String {
    blake3::hash(session_id.as_str().as_bytes())
        .to_hex()
        .to_string()
}

fn claim_path(session_id: &tau_proto::SessionId) -> PathBuf {
    claims_dir()
        .join(session_key(session_id))
        .with_extension(CLAIM_EXTENSION)
}

/// Returns the deterministic runtime path stem for one session.
#[must_use]
pub fn harness_path_for_session(session_id: &tau_proto::SessionId) -> PathBuf {
    sockets_dir().join(session_key(session_id))
}

/// Returns the socket pathname for a deterministic session stem.
#[must_use]
pub fn socket_path(harness_path: &Path) -> PathBuf {
    harness_path.with_extension(SOCKET_EXTENSION)
}

/// Acquires lifetime runtime ownership for one exact session.
pub fn claim_session(
    project_root: &Path,
    session_id: &tau_proto::SessionId,
) -> io::Result<SessionClaim> {
    prepare_harnesses_dir()?;
    validate_socket_path(&socket_path(&harness_path_for_session(session_id)))?;
    verify_runtime_lock_support()?;
    let claim_path = claim_path(session_id);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(&claim_path)?;
    validate_claim_file(&file, &claim_path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    let identity = FileIdentity::from_metadata(&file.metadata()?);
    validate_path_identity(&claim_path, identity)?;
    Ok(SessionClaim {
        file,
        identity,
        claim_path,
        socket_path: socket_path(&harness_path_for_session(session_id)),
        record: ClaimRecord {
            version: CLAIM_VERSION,
            session_id: session_id.clone(),
            project_root: project_root.to_path_buf(),
            peer_entrypoint: false,
        },
    })
}

/// Acquires a session claim under an explicit runtime parent.
///
/// This is intended for embedded hosts and hermetic integration tests that do
/// not use the ambient `XDG_RUNTIME_DIR`.
pub fn claim_session_in(
    runtime_parent: &Path,
    project_root: &Path,
    session_id: &tau_proto::SessionId,
) -> io::Result<SessionClaim> {
    with_runtime_dir(Some(runtime_parent), || {
        claim_session(project_root, session_id)
    })
}

fn validate_socket_path(path: &Path) -> io::Result<()> {
    // Linux `sockaddr_un.sun_path` reserves one byte for the trailing NUL.
    if path.as_os_str().as_bytes().len() >= 108 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime socket path is too long: {}", path.display()),
        ));
    }
    Ok(())
}

fn verify_runtime_lock_support() -> io::Result<()> {
    reject_known_network_filesystem(&claims_dir())?;
    let path = claims_dir().join(format!(".flock-test-{:016x}", rand::random::<u64>()));
    let first = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    let second = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    fs2::FileExt::try_lock_exclusive(&first)?;
    let result = fs2::FileExt::try_lock_exclusive(&second);
    let _ = std::fs::remove_file(&path);
    match result {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Ok(()) => Err(io::Error::other(
            "runtime filesystem does not enforce independent-open flock exclusion",
        )),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn reject_known_network_filesystem(path: &Path) -> io::Result<()> {
    use std::ffi::CString;

    let encoded = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "runtime path contains NUL"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `encoded` is NUL terminated and `stats` points to writable storage.
    if unsafe { libc::statfs(encoded.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful `statfs` initialized the output structure.
    let kind = unsafe { stats.assume_init() }.f_type as u64;
    const NFS_SUPER_MAGIC: u64 = 0x6969;
    const SMB_SUPER_MAGIC: u64 = 0x517B;
    const CIFS_MAGIC_NUMBER: u64 = 0xFF53_4D42;
    const CODA_SUPER_MAGIC: u64 = 0x7375_7245;
    const AFS_SUPER_MAGIC: u64 = 0x5346_414F;
    if matches!(
        kind,
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | CIFS_MAGIC_NUMBER | CODA_SUPER_MAGIC | AFS_SUPER_MAGIC
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime claims require a local filesystem with coherent flock semantics",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reject_known_network_filesystem(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn validate_claim_file(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_euid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("invalid runtime claim `{}`", path.display()),
        ));
    }
    Ok(())
}

fn validate_path_identity(path: &Path, expected: FileIdentity) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || FileIdentity::from_metadata(&metadata) != expected {
        return Err(io::Error::other("runtime claim pathname changed"));
    }
    Ok(())
}

fn read_claim(file: &mut File) -> io::Result<ClaimRecord> {
    if file.metadata()?.len() > MAX_CLAIM_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime claim record is too large",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_CLAIM_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CLAIM_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime claim record is too large",
        ));
    }
    let record: ClaimRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.version != CLAIM_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported runtime claim version",
        ));
    }
    Ok(record)
}

/// Resolves one exact running session to its deterministic socket stem.
pub fn find_harness_for_session(
    session_id: &str,
) -> Result<Option<PathBuf>, FindHarnessForSessionError> {
    let session_id = tau_proto::SessionId::parse(session_id).map_err(|_| {
        FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        }
    })?;
    find_harness_for_session_until(
        session_id.as_str(),
        Instant::now() + DISCOVERY_TIMEOUT,
        &AtomicBool::new(false),
    )
}

/// Resolves one exact running session within an absolute deadline.
pub(crate) fn find_harness_for_session_until(
    session_id: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Option<PathBuf>, FindHarnessForSessionError> {
    #[cfg(test)]
    if let Some(path) = TEST_SESSION_HARNESSES
        .lock()
        .expect("test registry")
        .get(session_id)
        .cloned()
    {
        return Ok(Some(path));
    }
    let parsed = tau_proto::SessionId::parse(session_id).map_err(|_| incomplete(session_id))?;
    let path = claim_path(&parsed);
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(incomplete(session_id));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(incomplete(session_id)),
    };
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(incomplete(session_id));
    }
    if validate_claim_file(&file, &path).is_err() {
        return Err(incomplete(session_id));
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(incomplete(session_id));
    }
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let identity =
                FileIdentity::from_metadata(&file.metadata().map_err(|_| incomplete(session_id))?);
            validate_path_identity(&path, identity).map_err(|_| incomplete(session_id))?;
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete(session_id));
            }
            Ok(None)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let identity =
                FileIdentity::from_metadata(&file.metadata().map_err(|_| incomplete(session_id))?);
            validate_path_identity(&path, identity).map_err(|_| incomplete(session_id))?;
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete(session_id));
            }
            let record = read_claim(&mut file).map_err(|_| incomplete(session_id))?;
            #[cfg(test)]
            if TEST_CANCEL_AFTER_CLAIM_READ.swap(false, Ordering::AcqRel) {
                cancelled.store(true, Ordering::Release);
            }
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete(session_id));
            }
            if record.session_id != parsed
                || session_key(&record.session_id) != session_key(&parsed)
            {
                return Err(incomplete(session_id));
            }
            let stem = harness_path_for_session(&parsed);
            probe_exact_session(&stem, &parsed, deadline, cancelled)
                .ok_or_else(|| incomplete(session_id))?;
            Ok(Some(stem))
        }
        Err(_) => Err(incomplete(session_id)),
    }
}

fn incomplete(session_id: &str) -> FindHarnessForSessionError {
    FindHarnessForSessionError::Incomplete {
        session_id: session_id.to_owned(),
    }
}

fn probe_exact_session(
    stem: &Path,
    session_id: &tau_proto::SessionId,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Option<RunningSession> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let timeout = probe_remaining(deadline, cancelled)?;
    let mut peer =
        tau_socket::SocketPeer::connect_with_io_timeout(socket_path(stem), timeout).ok()?;
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    peer.set_write_timeout(probe_remaining(deadline, cancelled)?)
        .ok()?;
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse("tau-runtime-probe").ok()?,
        client_kind: tau_proto::ClientKind::Ui,
        expected_session_id: Some(session_id.clone()),
        capabilities: Vec::new(),
    }))
    .ok()?;
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    match peer
        .recv_timeout(probe_remaining(deadline, cancelled)?)
        .ok()?
    {
        tau_socket::SocketReceive::Message {
            message: tau_proto::HarnessOutputMessage::SessionAccepted(accepted),
        } if accepted.session_id == *session_id => {}
        _ => return None,
    }
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let request_id = "runtime-probe".to_owned();
    peer.set_write_timeout(probe_remaining(deadline, cancelled)?)
        .ok()?;
    peer.send(&tau_proto::HarnessInputMessage::GetCurrentSession(
        tau_proto::GetCurrentSession {
            request_id: request_id.clone(),
        },
    ))
    .ok()?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        match peer
            .recv_timeout(probe_remaining(deadline, cancelled)?)
            .ok()?
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::CurrentSessionResult(result),
            } if result.request_id == request_id && result.session_id == *session_id => {
                return Some(RunningSession {
                    session_id: result.session_id,
                    project_root: result.project_root,
                });
            }
            tau_socket::SocketReceive::Message { .. } => {}
            _ => return None,
        }
    }
}

/// Lists every contended exact session claim and verifies its responder.
pub fn list_running_sessions() -> io::Result<Vec<RunningSession>> {
    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    let permit = DiscoveryCallPermit::try_acquire()
        .ok_or_else(|| io::Error::other("running-session discovery is busy"))?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let scan_cancelled = Arc::clone(&cancelled);
    let scan_permit = permit.clone();
    let scan_dir = claims_dir();
    let (scan_tx, scan_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _permit = scan_permit;
        #[cfg(test)]
        let _worker = DiscoveryWorkerGuard::new();
        let result = list_running_claim_records_until(&scan_dir, deadline, &scan_cancelled);
        let _ = scan_tx.send(result);
    });
    let records = deadline
        .checked_duration_since(Instant::now())
        .and_then(|remaining| scan_rx.recv_timeout(remaining).ok())
        .and_then(Result::ok)
        .ok_or_else(|| {
            cancelled.store(true, Ordering::Release);
            io::Error::other("could not list every running session claim")
        })?;
    let mut sessions = Vec::with_capacity(records.len());
    for record in records {
        let stem = harness_path_for_session(&record.session_id);
        let running = probe_exact_session(&stem, &record.session_id, deadline, &cancelled)
            .ok_or_else(|| io::Error::other("contended runtime claim is not responding"))?;
        sessions.push(running);
    }
    drop(permit);
    sessions.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then(left.project_root.cmp(&right.project_root))
    });
    Ok(sessions)
}

/// Discovers exact live sessions that advertise an inter-harness entrypoint.
pub(crate) fn discover_peer_sessions(
    query: Option<&str>,
    limit: usize,
    current_session_id: &str,
    permit: DiscoveryCallPermit,
) -> PeerSessionSnapshot {
    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    let cancelled = Arc::new(AtomicBool::new(false));
    let scan_dir = claims_dir();
    let socket_dir = sockets_dir();
    let scan_cancelled = Arc::clone(&cancelled);
    let scan_permit = permit.clone();
    let (scan_tx, scan_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _permit = scan_permit;
        #[cfg(test)]
        let _worker = DiscoveryWorkerGuard::new();
        let result = list_running_claim_records_until(&scan_dir, deadline, &scan_cancelled);
        let _ = scan_tx.send(result);
    });
    let scan_result = deadline
        .checked_duration_since(Instant::now())
        .and_then(|remaining| scan_rx.recv_timeout(remaining).ok());
    let Some(Ok(records)) = scan_result else {
        cancelled.store(true, Ordering::Release);
        discovery_probe_slots().1.notify_all();
        return incomplete_peer_snapshot();
    };

    let queue = Arc::new(Mutex::new(
        records
            .into_iter()
            .filter(|record| record.peer_entrypoint)
            .map(|record| {
                let stem = socket_dir.join(session_key(&record.session_id));
                (record, stem)
            })
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let workers = MAX_DISCOVERY_PROBES.min(queue.lock().expect("queue poisoned").len());
    let (tx, rx) = mpsc::channel();
    let mut available = Vec::new();
    let mut incomplete = false;
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            let cancelled = Arc::clone(&cancelled);
            scope.spawn(move || {
                #[cfg(test)]
                let _worker = DiscoveryWorkerGuard::new();
                loop {
                    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                        break;
                    }
                    let Some(slot) = DiscoveryProbeSlot::acquire(deadline, &cancelled) else {
                        break;
                    };
                    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                        drop(slot);
                        break;
                    }
                    let candidate = queue.lock().expect("queue poisoned").pop_front();
                    let Some((record, stem)) = candidate else {
                        break;
                    };
                    let outcome =
                        probe_peer_entrypoint(&stem, &record.session_id, deadline, &cancelled);
                    if tx.send((record, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                incomplete = true;
                break;
            };
            match rx.recv_timeout(remaining) {
                Ok((record, PeerProbeOutcome::Available)) => available.push(record),
                Ok((_, PeerProbeOutcome::Unavailable)) => {}
                Ok((_, PeerProbeOutcome::Incomplete)) => incomplete = true,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    incomplete = true;
                    break;
                }
            }
        }
        if !queue.lock().expect("queue poisoned").is_empty() {
            incomplete = true;
        }
        cancelled.store(true, Ordering::Release);
        discovery_probe_slots().1.notify_all();
    });
    drop(permit);
    if incomplete {
        return incomplete_peer_snapshot();
    }

    let needle = query.map(str::to_lowercase);
    let mut projected = available
        .into_iter()
        .filter_map(|record| {
            let project_label = record
                .project_root
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned);
            let matches = needle.as_ref().is_none_or(|needle| {
                record.session_id.as_str().to_lowercase().contains(needle)
                    || project_label
                        .as_ref()
                        .is_some_and(|label| label.to_lowercase().contains(needle))
            });
            matches.then(|| PeerSession {
                current: record.session_id.as_str() == current_session_id,
                session_id: record.session_id.to_string(),
                project_label,
            })
        })
        .collect::<Vec<_>>();
    projected.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    let limit = limit.min(SESSION_DISCOVERY_MAX_RESULTS);
    let truncated = projected.len() > limit;
    projected.truncate(limit);
    PeerSessionSnapshot {
        sessions: projected,
        truncated,
        scan_truncated: false,
    }
}

fn incomplete_peer_snapshot() -> PeerSessionSnapshot {
    PeerSessionSnapshot {
        sessions: Vec::new(),
        truncated: false,
        scan_truncated: true,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerProbeOutcome {
    Available,
    Unavailable,
    Incomplete,
}

fn probe_peer_entrypoint(
    stem: &Path,
    session_id: &tau_proto::SessionId,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> PeerProbeOutcome {
    if cancelled.load(Ordering::Acquire) {
        return PeerProbeOutcome::Incomplete;
    }
    let Some(timeout) = probe_remaining(deadline, cancelled) else {
        return PeerProbeOutcome::Incomplete;
    };
    let Ok(mut peer) = tau_socket::SocketPeer::connect_with_io_timeout(
        socket_path(stem),
        timeout.min(PROBE_TIMEOUT),
    ) else {
        return PeerProbeOutcome::Incomplete;
    };
    if cancelled.load(Ordering::Acquire) {
        return PeerProbeOutcome::Incomplete;
    }
    let Ok(client_name) =
        tau_proto::ExtensionName::parse(crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME)
    else {
        return PeerProbeOutcome::Incomplete;
    };
    let Some(write_timeout) = probe_remaining(deadline, cancelled) else {
        return PeerProbeOutcome::Incomplete;
    };
    if peer.set_write_timeout(write_timeout).is_err() {
        return PeerProbeOutcome::Incomplete;
    }
    if peer
        .send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name,
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: Some(session_id.clone()),
            capabilities: Vec::new(),
        }))
        .is_err()
        || cancelled.load(Ordering::Acquire)
    {
        return PeerProbeOutcome::Incomplete;
    }
    let Some(receive_timeout) = probe_remaining(deadline, cancelled) else {
        return PeerProbeOutcome::Incomplete;
    };
    if !matches!(
        peer.recv_timeout(receive_timeout),
        Ok(tau_socket::SocketReceive::Message {
            message: tau_proto::HarnessOutputMessage::SessionAccepted(accepted),
        }) if accepted.session_id == *session_id
    ) || cancelled.load(Ordering::Acquire)
    {
        return PeerProbeOutcome::Incomplete;
    }
    let request_id = format!("peer-probe-{}", std::process::id());
    let Some(write_timeout) = probe_remaining(deadline, cancelled) else {
        return PeerProbeOutcome::Incomplete;
    };
    if peer.set_write_timeout(write_timeout).is_err() {
        return PeerProbeOutcome::Incomplete;
    }
    if peer
        .send(&tau_proto::HarnessInputMessage::PeerSessionProbe(
            tau_proto::PeerSessionProbe {
                request_id: request_id.clone(),
                session_id: session_id.clone(),
            },
        ))
        .is_err()
        || cancelled.load(Ordering::Acquire)
    {
        return PeerProbeOutcome::Incomplete;
    }
    let Some(receive_timeout) = probe_remaining(deadline, cancelled) else {
        return PeerProbeOutcome::Incomplete;
    };
    match peer.recv_timeout(receive_timeout) {
        Ok(tau_socket::SocketReceive::Message {
            message: tau_proto::HarnessOutputMessage::PeerSessionProbeResult(result),
        }) if result.request_id == request_id && result.available => PeerProbeOutcome::Available,
        Ok(tau_socket::SocketReceive::Message {
            message: tau_proto::HarnessOutputMessage::PeerSessionProbeResult(result),
        }) if result.request_id == request_id => PeerProbeOutcome::Unavailable,
        _ => PeerProbeOutcome::Incomplete,
    }
}

fn probe_remaining(deadline: Instant, cancelled: &AtomicBool) -> Option<Duration> {
    probe_remaining_at(deadline, cancelled, Instant::now())
}

fn probe_remaining_at(deadline: Instant, cancelled: &AtomicBool, now: Instant) -> Option<Duration> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    deadline
        .checked_duration_since(now)
        .map(|remaining| remaining.min(PROBE_TIMEOUT))
}

fn list_running_claim_records_until(
    claims_directory: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<ClaimRecord>, ()> {
    #[cfg(test)]
    {
        let delay_ms = TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("scan delay lock poisoned")
            .as_ref()
            .filter(|(path, _)| path == claims_directory)
            .map(|(_, delay_ms)| *delay_ms);
        if let Some(delay_ms) = delay_ms {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(());
    }
    let entries = match std::fs::read_dir(claims_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(()),
    };
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    let mut visited = 0_usize;
    for entry in entries.take(MAX_DIRECTORY_ENTRIES + 1) {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(());
        }
        let path = entry.map_err(|_| ())?.path();
        visited += 1;
        if MAX_DIRECTORY_ENTRIES < visited {
            return Err(());
        }
        if path.extension().and_then(|value| value.to_str()) != Some(CLAIM_EXTENSION) {
            continue;
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options.open(&path).map_err(|_| ())?;
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(());
        }
        validate_claim_file(&file, &path).map_err(|_| ())?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(()),
        }
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(());
        }
        let record = read_claim(&mut file).map_err(|_| ())?;
        let expected_name = format!("{}.{}", session_key(&record.session_id), CLAIM_EXTENSION);
        if path.file_name().and_then(|value| value.to_str()) != Some(expected_name.as_str()) {
            return Err(());
        }
        if !seen.insert(record.session_id.clone()) {
            return Err(());
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
fn list_running_claim_records() -> Result<Vec<ClaimRecord>, ()> {
    list_running_claim_records_until(
        &claims_dir(),
        Instant::now() + DISCOVERY_TIMEOUT,
        &AtomicBool::new(false),
    )
}

fn discovery_probe_slots() -> &'static (Mutex<usize>, Condvar) {
    static SLOTS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    SLOTS.get_or_init(|| (Mutex::new(MAX_DISCOVERY_PROBES), Condvar::new()))
}

struct DiscoveryProbeSlot;

impl DiscoveryProbeSlot {
    fn acquire(deadline: Instant, cancelled: &AtomicBool) -> Option<Self> {
        let (slots, changed) = discovery_probe_slots();
        let mut available = slots.lock().expect("probe slots poisoned");
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if let Some(next) = available.checked_sub(1) {
                *available = next;
                return Some(Self);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timeout) = changed
                .wait_timeout(available, remaining)
                .expect("probe slot wait poisoned");
            available = next;
            if timeout.timed_out() {
                return None;
            }
        }
    }
}

impl Drop for DiscoveryProbeSlot {
    fn drop(&mut self) {
        let (slots, changed) = discovery_probe_slots();
        *slots.lock().expect("probe slots poisoned") += 1;
        changed.notify_one();
    }
}

#[cfg(test)]
struct DiscoveryWorkerGuard;

#[cfg(test)]
impl DiscoveryWorkerGuard {
    fn new() -> Self {
        ACTIVE_DISCOVERY_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(test)]
impl Drop for DiscoveryWorkerGuard {
    fn drop(&mut self) {
        ACTIVE_DISCOVERY_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
static TEST_SESSION_HARNESSES: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registers one exact test-only session socket stem until the guard drops.
#[cfg(test)]
pub(crate) fn register_test_session_harness(session_id: &str, path: PathBuf) -> impl Drop + use<> {
    TEST_SESSION_HARNESSES
        .lock()
        .expect("test registry")
        .insert(session_id.to_owned(), path);
    struct Guard(String);
    impl Drop for Guard {
        fn drop(&mut self) {
            TEST_SESSION_HARNESSES
                .lock()
                .expect("test registry")
                .remove(&self.0);
        }
    }
    Guard(session_id.to_owned())
}

fn ensure_private_runtime_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != current_euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("invalid runtime directory `{}`", path.display()),
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[allow(unsafe_code)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and reads process credentials only.
    unsafe { libc::geteuid() }
}
