//! Daemon runtime directory management.
//!
//! Each discoverable harness daemon gets one socket plus one adjacent metadata
//! file under `$XDG_RUNTIME_DIR/tau/harnesses/`:
//!
//! - `<pid>.sock` — Unix socket for client connections
//! - `<pid>.json` — daemon metadata used for discovery
//!
//! Metadata-based discovery enumerates `*.sock`, reads matching `*.json`, then
//! verifies liveness. Running-session listing instead treats sockets as
//! candidates and asks each responsive harness for its in-memory session
//! identity.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const HARNESSES_DIR: &str = "harnesses";
const SOCK_EXTENSION: &str = "sock";
const METADATA_EXTENSION: &str = "json";
const SESSION_DISCOVERY_MAX_CANDIDATES: usize = 128;
const SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES: usize = 4_096;
const SESSION_DISCOVERY_MAX_METADATA_BYTES: u64 = 16 * 1024;
const SESSION_DISCOVERY_MAX_PROBES: usize = 8;
const SESSION_DISCOVERY_MAX_CALLS: usize = 8;
const SESSION_DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const SESSION_DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(2);
// Keep at zero per `DECISION-no-backward-compatibility`.
const DAEMON_METADATA_VERSION: u32 = 0;

/// Maximum number of sessions returned by one discovery request.
pub const SESSION_DISCOVERY_MAX_RESULTS: usize = 50;

static ACTIVE_DISCOVERY_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Authoritative current identity reported by one responsive harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningSession {
    /// Harness-owned current session id at probe handling time.
    pub session_id: tau_proto::SessionId,
    /// Absolute canonical project root captured when the harness started.
    pub project_root: PathBuf,
}

/// Non-queued admission for one top-level session discovery call.
#[derive(Clone)]
pub(crate) struct DiscoveryCallPermit {
    /// Shared lease keeps the global slot charged through isolated storage
    /// work.
    _lease: Arc<DiscoveryCallLease>,
}

struct DiscoveryCallLease;

impl DiscoveryCallPermit {
    /// Acquires one process-wide call slot or rejects immediately.
    pub(crate) fn try_acquire() -> Option<Self> {
        #[allow(
            deprecated,
            reason = "AtomicUsize::try_update requires Rust 1.95, above the workspace MSRV"
        )]
        ACTIVE_DISCOVERY_CALLS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < SESSION_DISCOVERY_MAX_CALLS).then_some(active + 1)
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
    /// Active session routing key.
    pub session_id: String,
    /// Basename-only project label.
    pub project_label: Option<String>,
    /// Whether this is the calling harness's current session.
    pub current: bool,
}

/// Bounded, racy peer-session snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerSessionSnapshot {
    /// Deterministically session-id-sorted matching sessions.
    pub sessions: Vec<PeerSession>,
    /// True when the result limit omitted additional matches.
    pub truncated: bool,
    /// True when the runtime candidate cap omitted files from the scan.
    pub scan_truncated: bool,
}

/// Metadata written next to one discoverable harness socket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DaemonMetadata {
    /// Metadata schema version.
    pub version: u32,
    /// Process id of the harness that wrote this metadata.
    pub pid: u32,
    /// Optional project root associated with the harness instance.
    pub project_root: Option<PathBuf>,
    /// Active/current session id presently bound to this daemon.
    ///
    /// This field is intentionally kept under the original name for runtime
    /// metadata compatibility. Harnesses that support `/session new` update it
    /// after every successful session switch.
    pub session_id: String,
    /// Untrusted discovery hint that this daemon currently advertises a peer
    /// entrypoint. Callers must still probe the live harness.
    #[serde(default)]
    pub peer_entrypoint: bool,
}

/// Error returned when session-based daemon discovery is ambiguous.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FindHarnessForSessionError {
    /// More than one live harness advertises the requested active session.
    Ambiguous {
        session_id: String,
        matches: Vec<PathBuf>,
    },
    /// The bounded scan or deadline ended before uniqueness was proven.
    Incomplete {
        /// Requested active session routing key.
        session_id: String,
    },
}

impl std::fmt::Display for FindHarnessForSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous {
                session_id,
                matches,
            } => write!(
                f,
                "multiple running harnesses advertise session `{session_id}`: {}",
                matches
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Incomplete { session_id } => write!(
                f,
                "bounded runtime lookup could not prove a unique daemon for session `{session_id}`"
            ),
        }
    }
}

impl std::error::Error for FindHarnessForSessionError {}

/// Returns the root runtime directory for all tau daemon instances.
#[must_use]
pub fn root_runtime_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = test_runtime_dir_override() {
        return dir.join("tau");
    }
    dirs::runtime_dir()
        .map(|dir| dir.join("tau"))
        .unwrap_or_else(|| {
            #[cfg(unix)]
            {
                PathBuf::from(format!("/tmp/tau-{}", current_euid()))
            }
            #[cfg(not(unix))]
            {
                let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
                PathBuf::from(format!("/tmp/tau-{user}"))
            }
        })
}

#[cfg(test)]
fn test_runtime_dir_override() -> Option<PathBuf> {
    TEST_RUNTIME_DIR.with(|dir| dir.borrow().clone())
}

/// Test-only thread-scoped runtime root override.
#[cfg(test)]
pub(crate) struct RuntimeDirOverride;

#[cfg(test)]
impl Drop for RuntimeDirOverride {
    fn drop(&mut self) {
        TEST_RUNTIME_DIR.with(|dir| *dir.borrow_mut() = None);
    }
}

/// Installs a thread-scoped runtime root used by discovery tests.
#[cfg(test)]
pub(crate) fn override_test_runtime_dir(path: &Path) -> RuntimeDirOverride {
    TEST_RUNTIME_DIR.with(|dir| *dir.borrow_mut() = Some(path.to_path_buf()));
    RuntimeDirOverride
}

#[cfg(test)]
thread_local! {
    static TEST_RUNTIME_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_CANCEL_AFTER_SESSION_METADATA_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Returns the directory containing discoverable harness sockets.
#[must_use]
pub fn harnesses_dir() -> PathBuf {
    root_runtime_dir().join(HARNESSES_DIR)
}

/// Returns the socket path for a harness path stem.
#[must_use]
pub fn socket_path(harness_path: &Path) -> PathBuf {
    harness_path.with_extension(SOCK_EXTENSION)
}

/// Returns the metadata path for a harness path stem.
#[must_use]
pub fn metadata_path(harness_path: &Path) -> PathBuf {
    harness_path.with_extension(METADATA_EXTENSION)
}

/// Paths and metadata for one daemon instance.
pub struct HarnessPaths {
    path: PathBuf,
    metadata: DaemonMetadata,
}

impl HarnessPaths {
    /// Returns the harness path stem shared by the socket and metadata files.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the socket path.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        socket_path(&self.path)
    }

    /// Set the peer-entrypoint discovery hint before writing metadata.
    pub fn set_peer_entrypoint(&mut self, enabled: bool) {
        self.metadata.peer_entrypoint = enabled;
    }

    /// Writes the daemon metadata. Must be called after the socket is bound.
    pub fn write_metadata(&self) -> Result<(), std::io::Error> {
        let json = serde_json::to_vec_pretty(&self.metadata).map_err(std::io::Error::other)?;
        std::fs::write(metadata_path(&self.path), json)
    }

    /// Removes the daemon socket and metadata.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(socket_path(&self.path));
        let _ = std::fs::remove_file(metadata_path(&self.path));
    }
}

/// Reads the metadata for a harness path stem.
#[must_use]
pub fn read_metadata(harness_path: &Path) -> Option<DaemonMetadata> {
    std::fs::read_to_string(metadata_path(harness_path))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn read_metadata_bounded(
    harness_path: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Option<DaemonMetadata> {
    use std::io::Read as _;

    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return None;
    }
    let path = metadata_path(harness_path);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file()
        || cancelled.load(Ordering::Acquire)
        || Instant::now() >= deadline
    {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return None;
    }
    if file.metadata().ok()?.len() > SESSION_DISCOVERY_MAX_METADATA_BYTES {
        return None;
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(SESSION_DISCOVERY_MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= SESSION_DISCOVERY_MAX_METADATA_BYTES)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

fn collect_directory_entries_bounded<T>(
    mut entries: impl Iterator<Item = T>,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<T>, ()> {
    let mut collected = Vec::new();
    while collected.len() < SESSION_DISCOVERY_MAX_CANDIDATES {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(());
        }
        let Some(entry) = entries.next() else {
            break;
        };
        collected.push(entry);
    }
    Ok(collected)
}

/// Discover live sessions that currently confirm an advertised peer entrypoint.
///
/// Runtime files are only untrusted candidates. Every returned entry completed
/// a narrow live-harness probe, and all filesystem, metadata, concurrency,
/// result, per-probe, and total work is bounded.
pub(crate) fn discover_peer_sessions(
    query: Option<&str>,
    limit: usize,
    current_session_id: &str,
    permit: DiscoveryCallPermit,
) -> PeerSessionSnapshot {
    let deadline = Instant::now() + SESSION_DISCOVERY_TOTAL_TIMEOUT;
    let cancelled = Arc::new(AtomicBool::new(false));
    let scan_dir = harnesses_dir();
    let scan_cancelled = Arc::clone(&cancelled);
    let scan_permit = permit.clone();
    let (scan_tx, scan_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _permit = scan_permit;
        #[cfg(test)]
        if let Some((_, delay_ms)) = TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned")
            .as_ref()
            .filter(|(path, _)| path == &scan_dir)
        {
            std::thread::sleep(Duration::from_millis(*delay_ms));
        }
        if scan_cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            drop(_permit);
            let _ = scan_tx.send((Vec::new(), true));
            return;
        }
        let entries = match std::fs::read_dir(scan_dir).map(|directory| {
            collect_directory_entries_bounded(directory, deadline, &scan_cancelled)
        }) {
            Ok(Ok(entries)) => entries,
            Ok(Err(())) => {
                drop(_permit);
                let _ = scan_tx.send((Vec::new(), true));
                return;
            }
            Err(_) => Vec::new(),
        };
        let scan_truncated = entries.len() == SESSION_DISCOVERY_MAX_CANDIDATES;
        let mut paths = entries
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|ext| ext.to_str()) == Some(SOCK_EXTENSION))
                    .then(|| path.with_extension(""))
            })
            .collect::<Vec<_>>();
        paths.sort();
        let candidates = paths
            .into_iter()
            .filter_map(|path| {
                let metadata = read_metadata_bounded(&path, deadline, &scan_cancelled)?;
                metadata.peer_entrypoint.then_some((path, metadata))
            })
            .collect::<Vec<_>>();
        drop(_permit);
        let _ = scan_tx.send((candidates, scan_truncated));
    });
    let scan_result = deadline
        .checked_duration_since(Instant::now())
        .and_then(|remaining| scan_rx.recv_timeout(remaining).ok());
    let Some((candidates, scan_truncated)) = scan_result else {
        cancelled.store(true, Ordering::Release);
        return PeerSessionSnapshot {
            sessions: Vec::new(),
            truncated: false,
            scan_truncated: true,
        };
    };
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(candidates)));
    let (tx, rx) = mpsc::channel();
    let workers = SESSION_DISCOVERY_MAX_PROBES.min(queue.lock().expect("queue poisoned").len());
    let mut live = Vec::new();
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
                    let Some((path, metadata)) = candidate else {
                        drop(slot);
                        break;
                    };
                    if probe_peer_entrypoint(&path, &metadata.session_id, deadline, &cancelled) {
                        let _ = tx.send(metadata);
                    }
                }
            });
        }
        drop(tx);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(metadata) => live.push(metadata),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    cancelled.store(true, Ordering::Release);
                    discovery_probe_slots().1.notify_all();
                    break;
                }
            }
        }
        cancelled.store(true, Ordering::Release);
        discovery_probe_slots().1.notify_all();
    });
    live.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    let mut ambiguous = std::collections::HashSet::new();
    for pair in live.windows(2) {
        if pair[0].session_id == pair[1].session_id {
            ambiguous.insert(pair[0].session_id.clone());
        }
    }
    let needle = query.map(str::to_lowercase);
    let mut sessions = live
        .into_iter()
        .filter(|metadata| !ambiguous.contains(&metadata.session_id))
        .filter_map(|metadata| {
            let project_label = metadata
                .project_root
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::to_owned);
            let matches = needle.as_ref().is_none_or(|needle| {
                metadata.session_id.to_lowercase().contains(needle)
                    || project_label
                        .as_ref()
                        .is_some_and(|label| label.to_lowercase().contains(needle))
            });
            matches.then(|| PeerSession {
                current: metadata.session_id == current_session_id,
                session_id: metadata.session_id,
                project_label,
            })
        })
        .collect::<Vec<_>>();
    let limit = limit.min(SESSION_DISCOVERY_MAX_RESULTS);
    let truncated = sessions.len() > limit;
    sessions.truncate(limit);
    PeerSessionSnapshot {
        sessions,
        truncated,
        scan_truncated,
    }
}

#[cfg(test)]
pub(crate) fn discover_peer_sessions_for_test(
    query: Option<&str>,
    limit: usize,
    current_session_id: &str,
) -> PeerSessionSnapshot {
    let _serial = TEST_DISCOVERY_SERIAL
        .lock()
        .expect("test discovery serial lock poisoned");
    discover_peer_sessions(
        query,
        limit,
        current_session_id,
        DiscoveryCallPermit::try_acquire().expect("discovery call permit"),
    )
}

fn probe_peer_entrypoint(
    harness_path: &Path,
    session_id: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> bool {
    let ProbeConnect::Connected(mut peer, probe_deadline) = connect_probe_peer(
        harness_path,
        deadline,
        cancelled,
        crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
        tau_proto::ClientKind::External,
    ) else {
        return false;
    };
    let request_id = format!("peer-probe-{}", std::process::id());
    if peer
        .send(&tau_proto::HarnessInputMessage::PeerSessionProbe(
            tau_proto::PeerSessionProbe {
                request_id: request_id.clone(),
                session_id: session_id.into(),
            },
        ))
        .is_err()
    {
        return false;
    }
    matches!(
        peer.recv_timeout(
            probe_deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO)
        ),
        Ok(tau_socket::SocketReceive::Message {
            message: tau_proto::HarnessOutputMessage::PeerSessionProbeResult(result),
        }) if result.request_id == request_id && result.available
    )
}

enum ProbeConnect {
    Connected(tau_socket::SocketPeer, Instant),
    Unresponsive,
    Infrastructure(std::io::Error),
}

fn connect_probe_peer(
    harness_path: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    client_name: tau_proto::ExtensionName,
    client_kind: tau_proto::ClientKind,
) -> ProbeConnect {
    let connect = || -> Option<Result<(tau_socket::SocketPeer, Instant), std::io::Error>> {
        let probe_deadline = deadline.min(Instant::now() + SESSION_DISCOVERY_PROBE_TIMEOUT);
        let timeout = probe_deadline.checked_duration_since(Instant::now())?;
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let mut peer = match tau_socket::SocketPeer::connect_with_io_timeout(
            socket_path(harness_path),
            timeout,
        ) {
            Ok(peer) => peer,
            Err(tau_socket::SocketTransportError::SpawnReader { source }) => {
                return Some(Err(source));
            }
            Err(_) => return None,
        };
        peer.set_write_timeout(probe_deadline.checked_duration_since(Instant::now())?)
            .ok()?;
        peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name,
            client_kind,
            capabilities: Vec::new(),
        }))
        .ok()?;
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        peer.set_write_timeout(probe_deadline.checked_duration_since(Instant::now())?)
            .ok()?;
        Some(Ok((peer, probe_deadline)))
    };
    match connect() {
        Some(Ok((peer, probe_deadline))) => ProbeConnect::Connected(peer, probe_deadline),
        Some(Err(error)) => ProbeConnect::Infrastructure(error),
        None => ProbeConnect::Unresponsive,
    }
}

fn discovery_probe_slots() -> &'static (Mutex<usize>, Condvar) {
    static SLOTS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    SLOTS.get_or_init(|| (Mutex::new(SESSION_DISCOVERY_MAX_PROBES), Condvar::new()))
}

struct DiscoveryProbeSlot;

impl DiscoveryProbeSlot {
    fn acquire(deadline: Instant, cancelled: &AtomicBool) -> Option<Self> {
        let (slots, available) = discovery_probe_slots();
        let mut count = slots.lock().expect("discovery probe slots poisoned");
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if *count > 0 {
                *count -= 1;
                return Some(Self);
            }
            let (next, timeout) = available
                .wait_timeout(count, remaining)
                .expect("discovery probe slots poisoned");
            count = next;
            if timeout.timed_out() {
                return None;
            }
        }
    }
}

impl Drop for DiscoveryProbeSlot {
    fn drop(&mut self) {
        let (slots, available) = discovery_probe_slots();
        *slots.lock().expect("discovery probe slots poisoned") += 1;
        available.notify_one();
    }
}

#[cfg(test)]
static ACTIVE_DISCOVERY_WORKERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static TEST_DISCOVERY_SERIAL: Mutex<()> = Mutex::new(());
#[cfg(test)]
static TEST_DISCOVERY_SCAN_DELAY: std::sync::LazyLock<Mutex<Option<(PathBuf, u64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

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

/// Reads the session id a running daemon at `harness_path` is bound to.
#[must_use]
pub fn read_session_id(harness_path: &Path) -> Option<String> {
    read_metadata(harness_path).map(|metadata| metadata.session_id)
}

/// Updates the active/current session id in an existing daemon metadata file.
pub fn update_session_id(harness_path: &Path, session_id: &str) -> Result<(), std::io::Error> {
    let mut metadata = read_metadata(harness_path)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "metadata missing"))?;
    metadata.session_id = session_id.to_owned();
    let json = serde_json::to_vec_pretty(&metadata).map_err(std::io::Error::other)?;
    std::fs::write(metadata_path(harness_path), json)
}

/// Creates paths and metadata for the current process.
pub fn prepare_harness_paths(
    project_root: &Path,
    session_id: &str,
) -> Result<HarnessPaths, std::io::Error> {
    let pid = std::process::id();
    let root_dir = root_runtime_dir();
    ensure_private_runtime_dir(&root_dir)?;
    let harnesses_dir = root_dir.join(HARNESSES_DIR);
    ensure_private_runtime_dir(&harnesses_dir)?;
    let path = harnesses_dir.join(pid.to_string());
    Ok(HarnessPaths {
        path,
        metadata: DaemonMetadata {
            version: DAEMON_METADATA_VERSION,
            pid,
            project_root: Some(project_root.to_path_buf()),
            session_id: session_id.to_owned(),
            peer_entrypoint: false,
        },
    })
}

fn ensure_private_runtime_dir(path: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory `{}` must not be a symlink",
                path.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("runtime path `{}` is not a directory", path.display()),
        ));
    }
    ensure_private_runtime_dir_platform(path, &metadata)
}

#[cfg(unix)]
fn ensure_private_runtime_dir_platform(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), std::io::Error> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let uid = current_euid();
    if metadata.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory `{}` is owned by uid {}, not current uid {uid}",
                path.display(),
                metadata.uid()
            ),
        ));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_runtime_dir_platform(
    _path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and only reads the process'
    // effective user id.
    unsafe { libc::geteuid() }
}

/// Finds a running harness daemon for the given project root.
///
/// Discovery verifies matching candidates by connecting to their sockets.
/// Runtime discovery never removes an unreachable candidate. A liveness check
/// and a pathname unlink cannot be made atomic with daemon startup and PID
/// reuse, so cleanup belongs to the daemon's owned shutdown path.
#[must_use]
pub fn find_harness_for_dir(project_root: &Path) -> Option<PathBuf> {
    let runtime_dir = harnesses_dir();
    if !runtime_dir.exists() {
        return None;
    }

    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let sock = entry.path();
        if sock.extension().and_then(|ext| ext.to_str()) != Some(SOCK_EXTENSION) {
            continue;
        }
        let harness_path = sock.with_extension("");

        let metadata = match read_metadata(&harness_path) {
            Some(metadata) => metadata,
            None => continue,
        };

        if metadata
            .project_root
            .as_deref()
            .is_some_and(|stored_root| paths_equal(stored_root, project_root))
            && verify_harness_running(&harness_path)
        {
            return Some(harness_path);
        }
    }

    None
}

/// Lists authoritative current identities reported by responsive harness
/// daemons.
///
/// Persisted session directories and runtime metadata are not lifecycle
/// authority. This non-destructive scan uses bounded runtime-directory
/// traversal only to locate socket candidates, then obtains each identity
/// through a bounded local control RPC answered from harness memory. Results
/// are sorted by session id and project root. One record is retained per
/// responsive harness, including indistinguishable duplicate identities, so
/// callers can detect multiple harnesses for one directory. Having no
/// responsive daemons produces an empty vector.
///
/// # Errors
///
/// Returns an error instead of a partial list when the bounded candidate scan
/// fails, process-wide discovery admission is busy, the scan worker cannot be
/// spawned, or the total probe deadline expires before every candidate is
/// resolved.
pub fn list_running_sessions() -> Result<Vec<RunningSession>, std::io::Error> {
    let permit = DiscoveryCallPermit::try_acquire()
        .ok_or_else(|| running_session_list_incomplete("runtime scan capacity is busy"))?;
    let deadline = Instant::now() + SESSION_DISCOVERY_TOTAL_TIMEOUT;
    let cancelled = Arc::new(AtomicBool::new(false));
    let scan_cancelled = Arc::clone(&cancelled);
    let runtime_dir = harnesses_dir();
    let scan_permit = permit.clone();
    let (scan_tx, scan_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tau-running-session-scan".to_owned())
        .spawn(move || {
            let _permit = scan_permit;
            let result = scan_running_session_candidates(&runtime_dir, deadline, &scan_cancelled);
            let _ = scan_tx.send(result);
        })
        .map_err(|error| {
            running_session_list_incomplete(&format!("could not spawn runtime scan: {error}"))
        })?;
    let scan_timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| running_session_list_incomplete("runtime scan timed out"))?;
    let mut candidates = match scan_rx.recv_timeout(scan_timeout) {
        Ok(result) => result?,
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            return Err(running_session_list_incomplete("runtime scan timed out"));
        }
    };
    candidates.sort();
    let mut sessions = Vec::new();
    for harness_path in candidates {
        if Instant::now() >= deadline {
            return Err(running_session_list_incomplete(
                "runtime probe deadline reached before every candidate",
            ));
        }
        match probe_current_session(&harness_path, deadline, &cancelled) {
            CurrentSessionProbe::Reported(session) => sessions.push(session),
            CurrentSessionProbe::Unresponsive => {}
            CurrentSessionProbe::DeadlineExpired => {
                return Err(running_session_list_incomplete(
                    "runtime probe deadline expired",
                ));
            }
            CurrentSessionProbe::Infrastructure(error) => {
                return Err(running_session_list_incomplete(&format!(
                    "runtime probe infrastructure failed: {error}"
                )));
            }
        }
    }
    sessions.sort_by(|left, right| {
        (&left.session_id, &left.project_root).cmp(&(&right.session_id, &right.project_root))
    });
    Ok(sessions)
}

fn scan_running_session_candidates(
    runtime_dir: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<PathBuf>, std::io::Error> {
    #[cfg(test)]
    let delay_ms = TEST_DISCOVERY_SCAN_DELAY
        .lock()
        .expect("test scan delay lock poisoned")
        .as_ref()
        .filter(|(path, _)| path == runtime_dir)
        .map(|(_, delay_ms)| *delay_ms);
    #[cfg(test)]
    if let Some(delay_ms) = delay_ms {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    if !runtime_dir.try_exists()? {
        return Ok(Vec::new());
    }
    let mut entries = std::fs::read_dir(runtime_dir)?;
    let mut candidates = Vec::new();
    for entries_visited in 0..=SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(running_session_list_incomplete("runtime scan timed out"));
        }
        let Some(entry) = entries.next() else {
            return Ok(candidates);
        };
        if entries_visited == SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
            return Err(running_session_list_incomplete(
                "runtime directory entry limit reached",
            ));
        }
        let socket_path = entry?.path();
        if socket_path.extension().and_then(|ext| ext.to_str()) == Some(SOCK_EXTENSION) {
            candidates.push(socket_path.with_extension(""));
        }
    }
    Ok(candidates)
}

enum CurrentSessionProbe {
    Reported(RunningSession),
    Unresponsive,
    DeadlineExpired,
    Infrastructure(std::io::Error),
}

fn probe_current_session(
    harness_path: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> CurrentSessionProbe {
    let reported = (|| {
        let (mut peer, probe_deadline) = match connect_probe_peer(
            harness_path,
            deadline,
            cancelled,
            "tau-session-list".into(),
            tau_proto::ClientKind::Ui,
        ) {
            ProbeConnect::Connected(peer, probe_deadline) => (peer, probe_deadline),
            ProbeConnect::Unresponsive => return None,
            ProbeConnect::Infrastructure(error) => {
                return Some(Err(error));
            }
        };
        let request_id = format!("current-session-{}", std::process::id());
        peer.send(&tau_proto::HarnessInputMessage::GetCurrentSession(
            tau_proto::GetCurrentSession {
                request_id: request_id.clone(),
            },
        ))
        .ok()?;
        loop {
            match peer
                .recv_timeout(probe_deadline.checked_duration_since(Instant::now())?)
                .ok()?
            {
                tau_socket::SocketReceive::Message {
                    message: tau_proto::HarnessOutputMessage::CurrentSessionResult(result),
                } if result.request_id == request_id => {
                    return Some(Ok(RunningSession {
                        session_id: result.session_id,
                        project_root: result.project_root,
                    }));
                }
                tau_socket::SocketReceive::Message {
                    message: tau_proto::HarnessOutputMessage::Disconnect(_),
                }
                | tau_socket::SocketReceive::Timeout
                | tau_socket::SocketReceive::Closed => return None,
                tau_socket::SocketReceive::Message { .. } => {}
            }
        }
    })();
    match reported {
        Some(Ok(session)) => CurrentSessionProbe::Reported(session),
        Some(Err(error)) => CurrentSessionProbe::Infrastructure(error),
        None if Instant::now() >= deadline => CurrentSessionProbe::DeadlineExpired,
        None => CurrentSessionProbe::Unresponsive,
    }
}

fn running_session_list_incomplete(reason: &str) -> std::io::Error {
    std::io::Error::other(format!("could not list all running sessions: {reason}"))
}

/// Finds the single live harness advertising `session_id` as its active
/// session.
///
/// Discovery verifies candidates by connecting to their sockets. A matching
/// unreachable record with a live or unverifiable metadata pid leaves
/// uniqueness unresolved. Dead unreachable records are ignored but preserved:
/// pathname deletion cannot be made atomic with liveness and identity checks
/// needed to exclude PID reuse. Returns `Ok(None)` when no live daemon
/// advertises the session and `Err` when uniqueness cannot be proven, including
/// ambiguous, truncated, and expired lookups.
pub fn find_harness_for_session(
    session_id: &str,
) -> Result<Option<PathBuf>, FindHarnessForSessionError> {
    find_harness_for_session_until(
        session_id,
        Instant::now() + SESSION_DISCOVERY_TOTAL_TIMEOUT,
        &AtomicBool::new(false),
    )
}

/// Finds one live session daemon with bounded traversal, metadata I/O, and an
/// absolute caller-owned deadline.
///
/// Cancellation returns [`FindHarnessForSessionError::Incomplete`].
pub(crate) fn find_harness_for_session_until(
    session_id: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Option<PathBuf>, FindHarnessForSessionError> {
    #[cfg(test)]
    if let Some(path) = TEST_SESSION_HARNESSES
        .lock()
        .expect("test session harness registry poisoned")
        .get(session_id)
        .cloned()
    {
        return Ok(Some(path));
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        });
    }
    let runtime_dir = harnesses_dir();
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        });
    }
    match runtime_dir.try_exists() {
        Ok(false) => return Ok(None),
        Err(_) => {
            return Err(FindHarnessForSessionError::Incomplete {
                session_id: session_id.to_owned(),
            });
        }
        Ok(true) => {}
    }

    let mut matches = Vec::new();
    let mut unresolved_match = false;
    let mut matching_candidates = 0;
    let mut entries_visited = 0;
    let mut scan_exhausted = false;
    {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(FindHarnessForSessionError::Incomplete {
                session_id: session_id.to_owned(),
            });
        }
        let Ok(entries) = std::fs::read_dir(&runtime_dir) else {
            return Err(incomplete_or_ambiguous(session_id, &matches));
        };
        for entry in entries.take(SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES) {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete_or_ambiguous(session_id, &matches));
            }
            entries_visited += 1;
            let Ok(entry) = entry else {
                return Err(incomplete_or_ambiguous(session_id, &matches));
            };
            let metadata_path = entry.path();
            if metadata_path.extension().and_then(|ext| ext.to_str()) != Some(METADATA_EXTENSION) {
                continue;
            }
            let harness_path = metadata_path.with_extension("");
            let metadata = match read_metadata_bounded(&harness_path, deadline, cancelled) {
                Some(metadata) => metadata,
                None if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline => {
                    return Err(incomplete_or_ambiguous(session_id, &matches));
                }
                None => {
                    if numeric_stem_liveness(&harness_path)
                        .is_some_and(|liveness| liveness != ProcessLiveness::Dead)
                    {
                        unresolved_match = true;
                    }
                    continue;
                }
            };
            #[cfg(test)]
            TEST_CANCEL_AFTER_SESSION_METADATA_READ.with(|enabled| {
                if enabled.replace(false) {
                    cancelled.store(true, Ordering::Release);
                }
            });
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete_or_ambiguous(session_id, &matches));
            }
            if metadata.session_id != session_id {
                continue;
            }
            matching_candidates += 1;
            if matching_candidates > SESSION_DISCOVERY_MAX_CANDIDATES {
                scan_exhausted = true;
                continue;
            }
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete_or_ambiguous(session_id, &matches));
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| incomplete_or_ambiguous(session_id, &matches))?;
            if tau_socket::SocketPeer::connect_with_io_timeout(
                socket_path(&harness_path),
                remaining,
            )
            .is_ok()
            {
                matches.push(harness_path);
            } else {
                match process_liveness(metadata.pid) {
                    ProcessLiveness::Dead => {}
                    ProcessLiveness::Running | ProcessLiveness::Unknown => {
                        unresolved_match = true;
                    }
                }
            }
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(incomplete_or_ambiguous(session_id, &matches));
            }
        }
        scan_exhausted |= entries_visited == SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES;
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(incomplete_or_ambiguous(session_id, &matches));
    }
    match matches.len() {
        2.. => Err(FindHarnessForSessionError::Ambiguous {
            session_id: session_id.to_owned(),
            matches,
        }),
        _ if scan_exhausted || unresolved_match => Err(FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        }),
        0 => Ok(None),
        1 => Ok(matches.pop()),
    }
}

/// Classifies an incomplete scan without losing already-proven ambiguity.
fn incomplete_or_ambiguous(
    session_id: &str,
    live_matches: &[PathBuf],
) -> FindHarnessForSessionError {
    if live_matches.len() >= 2 {
        FindHarnessForSessionError::Ambiguous {
            session_id: session_id.to_owned(),
            matches: live_matches.to_vec(),
        }
    } else {
        FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        }
    }
}

/// Returns liveness only for a conventional numeric daemon path stem.
fn numeric_stem_liveness(harness_path: &Path) -> Option<ProcessLiveness> {
    harness_path
        .file_name()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .map(process_liveness)
}

/// Conservative process-liveness result used for unreachable session claims.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessLiveness {
    /// The process entry is observable.
    Running,
    /// Procfs is observable and the process entry is absent.
    Dead,
    /// Process liveness cannot be proven.
    Unknown,
}

#[cfg(test)]
static TEST_SESSION_HARNESSES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, PathBuf>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Registers a real test harness socket for worker-thread session lookup.
#[cfg(test)]
pub(crate) fn register_test_session_harness(
    session_id: &str,
    harness_path: PathBuf,
) -> TestSessionHarnessGuard {
    TEST_SESSION_HARNESSES
        .lock()
        .expect("test session harness registry poisoned")
        .insert(session_id.to_owned(), harness_path);
    TestSessionHarnessGuard {
        session_id: session_id.to_owned(),
    }
}

/// Removes a worker-visible test session registration when dropped.
#[cfg(test)]
pub(crate) struct TestSessionHarnessGuard {
    /// Registered session key.
    session_id: String,
}

#[cfg(test)]
impl Drop for TestSessionHarnessGuard {
    fn drop(&mut self) {
        TEST_SESSION_HARNESSES
            .lock()
            .expect("test session harness registry poisoned")
            .remove(&self.session_id);
    }
}

/// Verifies that a daemon is actually running by connecting to its
/// socket.
pub(crate) fn verify_harness_running(harness_path: &Path) -> bool {
    UnixStream::connect(socket_path(harness_path)).is_ok()
}

#[cfg(target_os = "linux")]
fn process_liveness(pid: u32) -> ProcessLiveness {
    process_liveness_at(Path::new("/proc"), pid)
}

#[cfg(target_os = "linux")]
fn process_liveness_at(proc_root: &Path, pid: u32) -> ProcessLiveness {
    if !std::fs::metadata(proc_root.join("self")).is_ok_and(|metadata| metadata.is_dir()) {
        return ProcessLiveness::Unknown;
    }
    match std::fs::symlink_metadata(proc_root.join(pid.to_string())) {
        Ok(_) => ProcessLiveness::Running,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProcessLiveness::Dead,
        Err(_) => ProcessLiveness::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
fn process_liveness(_pid: u32) -> ProcessLiveness {
    // Tau forbids unsafe code, so avoid libc `kill(pid, 0)` here. Preserving
    // files is the conservative fallback: discovery still never returns a
    // candidate whose socket probe failed, but non-Linux platforms may retain
    // stale runtime files until a future safe liveness backend is added.
    ProcessLiveness::Unknown
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a_canon), Ok(b_canon)) => a_canon == b_canon,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::process::{Child, Command};

    use tempfile::TempDir;

    use super::*;

    fn runtime_override(temp: &TempDir) -> RuntimeDirOverride {
        override_test_runtime_dir(temp.path())
    }

    fn discover_for_test(
        query: Option<&str>,
        limit: usize,
        current_session_id: &str,
    ) -> PeerSessionSnapshot {
        discover_peer_sessions_for_test(query, limit, current_session_id)
    }

    fn write_peer_metadata(path: &Path, session_id: &str, project_root: &Path, opted_in: bool) {
        std::fs::write(
            metadata_path(path),
            serde_json::to_vec(&DaemonMetadata {
                version: DAEMON_METADATA_VERSION,
                pid: std::process::id(),
                project_root: Some(project_root.to_path_buf()),
                session_id: session_id.to_owned(),
                peer_entrypoint: opted_in,
            })
            .expect("metadata json"),
        )
        .expect("write metadata");
    }

    fn spawn_probe_daemon(
        path: &Path,
        expected_session: &str,
        requests: usize,
        respond: bool,
    ) -> std::thread::JoinHandle<()> {
        let listener = tau_socket::SocketListener::bind(socket_path(path)).expect("probe listener");
        let expected_session = expected_session.to_owned();
        std::thread::spawn(move || {
            for _ in 0..requests {
                let mut client = listener.accept().expect("accept probe");
                assert!(matches!(
                    client.recv().expect("hello"),
                    Some(tau_proto::HarnessInputMessage::Hello(_))
                ));
                let Some(tau_proto::HarnessInputMessage::PeerSessionProbe(probe)) =
                    client.recv().expect("probe")
                else {
                    panic!("expected peer session probe");
                };
                assert_eq!(probe.session_id, expected_session);
                if respond {
                    client
                        .send(&tau_proto::HarnessOutputMessage::PeerSessionProbeResult(
                            tau_proto::PeerSessionProbeResult {
                                request_id: probe.request_id,
                                available: true,
                            },
                        ))
                        .expect("probe result");
                } else {
                    std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT * 2);
                }
            }
        })
    }

    fn spawn_current_session_daemon(path: &Path, session_id: &str) -> std::thread::JoinHandle<()> {
        let listener =
            tau_socket::SocketListener::bind(socket_path(path)).expect("current-session listener");
        let session_id = tau_proto::SessionId::from(session_id);
        let project_root = path
            .parent()
            .expect("runtime path parent")
            .canonicalize()
            .expect("canonical project root");
        std::thread::spawn(move || {
            let mut client = listener.accept().expect("accept current-session probe");
            assert!(matches!(
                client.recv().expect("hello"),
                Some(tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
                    client_kind: tau_proto::ClientKind::Ui,
                    ..
                }))
            ));
            let Some(tau_proto::HarnessInputMessage::GetCurrentSession(request)) =
                client.recv().expect("current-session request")
            else {
                panic!("expected current-session request");
            };
            client
                .send(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                    tau_proto::CurrentSessionResult {
                        request_id: "unrelated-request".to_owned(),
                        session_id: "wrong-session".into(),
                        project_root: PathBuf::from("/wrong/project"),
                    },
                ))
                .expect("unrelated current-session result");
            client
                .send(&tau_proto::HarnessOutputMessage::CurrentSessionResult(
                    tau_proto::CurrentSessionResult {
                        request_id: request.request_id,
                        session_id,
                        project_root,
                    },
                ))
                .expect("current-session result");
        })
    }

    /// Exercises live opt-in confirmation against two daemon sockets while
    /// proving a non-opted daemon is not probed and full project paths are
    /// never returned.
    #[test]
    fn peer_discovery_two_daemons_requires_opt_in_and_redacts_project_root() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let opted = harnesses_dir().join("a");
        let private = harnesses_dir().join("b");
        let opted_daemon = spawn_probe_daemon(&opted, "live-session", 1, true);
        let _private_listener =
            tau_socket::SocketListener::bind(socket_path(&private)).expect("private listener");
        write_peer_metadata(
            &opted,
            "live-session",
            Path::new("/secret/parent/public-project"),
            true,
        );
        write_peer_metadata(
            &private,
            "private-session",
            Path::new("/secret/parent/private-project"),
            false,
        );

        let snapshot = discover_for_test(None, 50, "live-session");

        assert_eq!(
            snapshot.sessions,
            vec![PeerSession {
                session_id: "live-session".to_owned(),
                project_label: Some("public-project".to_owned()),
                current: true,
            }]
        );
        assert!(!format!("{snapshot:?}").contains("/secret/"));
        opted_daemon.join().expect("opted daemon");
    }

    /// Ensures a daemon's rewritten active session is authoritative and stale
    /// metadata is omitted from the same discovery snapshot.
    #[test]
    fn peer_discovery_tracks_active_session_and_omits_stale_record() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let live = harnesses_dir().join("live");
        let stale = harnesses_dir().join("stale");
        let daemon = spawn_probe_daemon(&live, "new-session", 1, true);
        write_peer_metadata(&live, "old-session", Path::new("/project"), true);
        update_session_id(&live, "new-session").expect("rewrite active session");
        write_peer_metadata(&stale, "stale-session", Path::new("/stale"), true);
        std::fs::write(socket_path(&stale), b"not a socket").expect("stale socket");

        let snapshot = discover_for_test(None, 50, "");

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].session_id, "new-session");
        daemon.join().expect("live daemon");
    }

    /// Ensures two live daemons claiming one routing key are conservatively
    /// omitted instead of exposing a nondeterministic peer destination.
    #[test]
    fn peer_discovery_omits_ambiguous_live_session() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let first = harnesses_dir().join("first");
        let second = harnesses_dir().join("second");
        let first_daemon = spawn_probe_daemon(&first, "same", 1, true);
        let second_daemon = spawn_probe_daemon(&second, "same", 1, true);
        write_peer_metadata(&first, "same", Path::new("/one"), true);
        write_peer_metadata(&second, "same", Path::new("/two"), true);

        assert!(discover_for_test(None, 50, "").sessions.is_empty());

        first_daemon.join().expect("first daemon");
        second_daemon.join().expect("second daemon");
    }

    /// Proves traversal counts every directory entry before filtering, so a
    /// directory dominated by unrelated files cannot cause unbounded scanning.
    #[test]
    fn peer_discovery_counts_non_candidates_toward_scan_bound() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        for index in 0..=SESSION_DISCOVERY_MAX_CANDIDATES {
            std::fs::write(harnesses_dir().join(format!("{index}.unrelated")), b"x")
                .expect("unrelated entry");
        }

        let snapshot = discover_for_test(None, 50, "");

        assert!(snapshot.scan_truncated);
        assert!(snapshot.sessions.is_empty());
    }

    /// Cancellation raised by one directory dequeue is observed before the
    /// iterator can perform another filesystem dequeue.
    #[test]
    fn bounded_directory_iteration_stops_before_post_cancel_dequeue() {
        let cancelled = AtomicBool::new(false);
        let dequeues = AtomicUsize::new(0);
        let entries = std::iter::from_fn(|| {
            dequeues.fetch_add(1, Ordering::AcqRel);
            cancelled.store(true, Ordering::Release);
            Some(())
        });

        assert!(
            collect_directory_entries_bounded(
                entries,
                Instant::now() + Duration::from_secs(1),
                &cancelled
            )
            .is_err()
        );
        assert_eq!(dequeues.load(Ordering::Acquire), 1);
    }

    /// Whole-call admission rejects a ninth concurrent coordinator without
    /// spawning or queueing and restores every slot after completion.
    #[test]
    fn peer_discovery_whole_call_admission_is_non_queued() {
        let _serial = TEST_DISCOVERY_SERIAL
            .lock()
            .expect("test discovery serial lock poisoned");
        let wait_deadline = Instant::now() + Duration::from_secs(1);
        while ACTIVE_DISCOVERY_CALLS.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < wait_deadline,
                "discovery lease did not retire"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let permits = (0..SESSION_DISCOVERY_MAX_CALLS)
            .map(|_| DiscoveryCallPermit::try_acquire().expect("discovery call slot"))
            .collect::<Vec<_>>();
        assert!(DiscoveryCallPermit::try_acquire().is_none());
        drop(permits);
        assert!(DiscoveryCallPermit::try_acquire().is_some());
    }

    /// A stalled storage scan cannot retain the caller beyond the total
    /// deadline; its bounded lease remains charged until the isolated
    /// worker exits.
    #[test]
    fn peer_discovery_slow_storage_isolated_by_total_deadline() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 2_100));
        let _serial = TEST_DISCOVERY_SERIAL
            .lock()
            .expect("test discovery serial lock poisoned");
        let started = Instant::now();

        let snapshot = discover_peer_sessions(
            None,
            50,
            "",
            DiscoveryCallPermit::try_acquire().expect("discovery call permit"),
        );

        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = None;
        assert!(started.elapsed() < Duration::from_millis(2_200));
        assert!(snapshot.sessions.is_empty());
        assert!(snapshot.scan_truncated);
        std::thread::sleep(Duration::from_millis(150));
    }

    /// Proves the metadata reader accepts the exact 16 KiB boundary and rejects
    /// one additional byte without parsing or allocating an unbounded payload.
    #[test]
    fn peer_discovery_metadata_byte_limit_is_inclusive_and_bounded() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let path = harnesses_dir().join("metadata-boundary");
        let base = serde_json::to_vec(&DaemonMetadata {
            version: DAEMON_METADATA_VERSION,
            pid: std::process::id(),
            project_root: None,
            session_id: "boundary".to_owned(),
            peer_entrypoint: true,
        })
        .expect("metadata");
        let mut exact = base.clone();
        exact.resize(SESSION_DISCOVERY_MAX_METADATA_BYTES as usize, b' ');
        std::fs::write(metadata_path(&path), &exact).expect("exact metadata");
        let cancelled = AtomicBool::new(false);
        assert!(
            read_metadata_bounded(&path, Instant::now() + Duration::from_secs(1), &cancelled)
                .is_some()
        );

        exact.push(b' ');
        std::fs::write(metadata_path(&path), exact).expect("oversized metadata");
        assert!(
            read_metadata_bounded(&path, Instant::now() + Duration::from_secs(1), &cancelled)
                .is_none()
        );
    }

    /// Proves result truncation remains explicit after live confirmation.
    #[test]
    fn peer_discovery_reports_result_truncation() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let count = SESSION_DISCOVERY_MAX_RESULTS + 1;
        let mut daemons = Vec::new();
        for index in 0..count {
            let path = harnesses_dir().join(format!("{index:03}"));
            let session = format!("session-{index:03}");
            daemons.push(spawn_probe_daemon(&path, &session, 1, true));
            write_peer_metadata(&path, &session, Path::new("/project"), true);
        }

        let snapshot = discover_for_test(None, SESSION_DISCOVERY_MAX_RESULTS, "");

        assert_eq!(snapshot.sessions.len(), SESSION_DISCOVERY_MAX_RESULTS);
        assert!(snapshot.truncated);
        for daemon in daemons {
            daemon.join().expect("probe daemon");
        }
    }

    /// Repeated timed-out probes must return only after scoped workers exit,
    /// preventing deadline cancellation from accumulating background workers.
    #[test]
    fn peer_discovery_deadline_cancellation_leaves_no_workers() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let calls = 3;
        let mut daemons = Vec::new();
        for index in 0..(SESSION_DISCOVERY_MAX_PROBES + 1) {
            let path = harnesses_dir().join(format!("blocked-{index}"));
            let session = format!("blocked-session-{index}");
            daemons.push(spawn_probe_daemon(&path, &session, calls, false));
            write_peer_metadata(&path, &session, Path::new("/project"), true);
        }

        for _ in 0..calls {
            assert!(discover_for_test(None, 50, "").sessions.is_empty());
            assert_eq!(ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire), 0);
        }
        for daemon in daemons {
            daemon.join().expect("blocked daemon");
        }
    }

    /// Saturated global probe slots keep candidates queued through the total
    /// deadline; cancellation prevents a later socket connect after return.
    #[test]
    fn peer_discovery_total_deadline_cancels_queued_probe_before_io() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let path = harnesses_dir().join("queued");
        let listener = UnixListener::bind(socket_path(&path)).expect("queued listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        write_peer_metadata(&path, "queued-session", Path::new("/project"), true);
        let _serial = TEST_DISCOVERY_SERIAL
            .lock()
            .expect("test discovery serial lock poisoned");
        let cancelled = AtomicBool::new(false);
        let slots = (0..SESSION_DISCOVERY_MAX_PROBES)
            .map(|_| {
                DiscoveryProbeSlot::acquire(Instant::now() + Duration::from_secs(5), &cancelled)
                    .expect("probe slot")
            })
            .collect::<Vec<_>>();
        let started = Instant::now();

        let snapshot = discover_peer_sessions(
            None,
            50,
            "",
            DiscoveryCallPermit::try_acquire().expect("discovery call permit"),
        );

        assert!(started.elapsed() < Duration::from_millis(2_200));
        assert!(snapshot.sessions.is_empty());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        assert_eq!(ACTIVE_DISCOVERY_WORKERS.load(Ordering::Acquire), 0);
        drop(slots);
    }

    /// Ensures daemon metadata's `session_id` field is the active session and
    /// can be updated in place without losing the stable pid/project fields.
    #[test]
    fn update_session_id_rewrites_active_session_metadata() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "old-session").expect("paths");
        paths.write_metadata().expect("write metadata");

        update_session_id(paths.path(), "new-session").expect("update session");

        let metadata = read_metadata(paths.path()).expect("metadata");
        assert_eq!(metadata.session_id, "new-session");
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(
            metadata.project_root.as_deref(),
            Some(project_root.as_path())
        );
    }

    /// Ensures running-session listing derives lifecycle from responsive daemon
    /// memory rather than persisted sessions, runtime metadata, or stale paths.
    #[test]
    fn running_session_list_includes_only_reachable_active_sessions() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let live = harnesses_dir().join("live");
        let stale = harnesses_dir().join(std::process::id().to_string());
        let daemon = spawn_current_session_daemon(&live, "running-session");
        write_peer_metadata(&stale, "historical-session", Path::new("/stale"), false);
        std::fs::write(socket_path(&stale), b"not a socket").expect("stale socket");

        assert_eq!(
            list_running_sessions().expect("running sessions"),
            vec![RunningSession {
                session_id: "running-session".into(),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            }]
        );
        daemon.join().expect("current-session daemon");
    }

    /// Ensures absence of runtime candidates is a successful empty,
    /// pipe-friendly listing rather than a synthesized placeholder row.
    #[test]
    fn running_session_list_is_empty_without_runtime_directory() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);

        assert!(
            list_running_sessions()
                .expect("running sessions")
                .is_empty()
        );
        assert!(!harnesses_dir().exists());
    }

    /// Ensures daemon memory remains authoritative when adjacent metadata is
    /// missing or unreadable during a rewrite or startup window.
    #[test]
    fn running_session_list_ignores_invalid_runtime_metadata() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let live = harnesses_dir().join("live");
        let daemon = spawn_current_session_daemon(&live, "authoritative-session");
        std::fs::write(metadata_path(&live), b"{").expect("invalid metadata");

        assert_eq!(
            list_running_sessions().expect("running sessions"),
            vec![RunningSession {
                session_id: "authoritative-session".into(),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            }]
        );
        daemon.join().expect("current-session daemon");
    }

    /// Ensures listing sorts responsive records while retaining
    /// indistinguishable identities so callers can detect multiple
    /// harnesses for one directory.
    #[test]
    fn running_session_list_sorts_and_retains_duplicate_identities() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let z = spawn_current_session_daemon(&harnesses_dir().join("a"), "z-session");
        let duplicate = spawn_current_session_daemon(&harnesses_dir().join("b"), "a-session");
        let a = spawn_current_session_daemon(&harnesses_dir().join("c"), "a-session");

        assert_eq!(
            list_running_sessions().expect("running sessions"),
            vec![
                RunningSession {
                    session_id: "a-session".into(),
                    project_root: harnesses_dir().canonicalize().expect("canonical root"),
                },
                RunningSession {
                    session_id: "a-session".into(),
                    project_root: harnesses_dir().canonicalize().expect("canonical root"),
                },
                RunningSession {
                    session_id: "z-session".into(),
                    project_root: harnesses_dir().canonicalize().expect("canonical root"),
                },
            ]
        );
        z.join().expect("z daemon");
        duplicate.join().expect("duplicate daemon");
        a.join().expect("a daemon");
    }

    /// Ensures the raw directory-entry bound fails the whole listing rather
    /// than returning a partial set after a stale-file flood.
    #[test]
    fn running_session_list_fails_at_directory_entry_bound() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        for index in 0..=SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
            std::fs::write(harnesses_dir().join(format!("junk-{index}")), b"junk")
                .expect("junk runtime entry");
        }

        let error = list_running_sessions().expect_err("bounded listing");
        assert!(
            error
                .to_string()
                .contains("runtime directory entry limit reached")
        );
    }

    /// Ensures one responsive non-Tau or stalled socket consumes only its
    /// per-probe budget and cannot hide a later responsive harness.
    #[test]
    fn running_session_list_continues_after_one_probe_timeout() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let blocked = harnesses_dir().join("a-blocked");
        let live = harnesses_dir().join("b-live");
        let blocked_listener =
            tau_socket::SocketListener::bind(socket_path(&blocked)).expect("blocked listener");
        let blocked_daemon = std::thread::spawn(move || {
            let _client = blocked_listener.accept().expect("accept blocked probe");
            std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT * 2);
        });
        let live_daemon = spawn_current_session_daemon(&live, "responsive-session");
        let started = Instant::now();

        assert_eq!(
            list_running_sessions().expect("running sessions"),
            vec![RunningSession {
                session_id: "responsive-session".into(),
                project_root: harnesses_dir().canonicalize().expect("canonical root"),
            }]
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        blocked_daemon.join().expect("blocked daemon");
        live_daemon.join().expect("live daemon");
    }

    /// Ensures a blocked runtime-directory operation is isolated from the
    /// caller's total deadline while retaining bounded discovery admission.
    #[test]
    fn running_session_list_isolates_slow_storage() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let _serial = TEST_DISCOVERY_SERIAL
            .lock()
            .expect("test discovery serial lock poisoned");
        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 2_100));
        let started = Instant::now();

        let error = list_running_sessions().expect_err("scan deadline");

        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = None;
        assert!(started.elapsed() < Duration::from_millis(2_200));
        assert!(error.to_string().contains("runtime scan timed out"));
    }

    /// Ensures expiry inside the final probe fails the whole listing rather
    /// than returning the ids collected before the global budget ran out.
    #[test]
    fn running_session_list_rejects_partial_result_on_final_probe_deadline() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(harnesses_dir()).expect("harnesses dir");
        let _serial = TEST_DISCOVERY_SERIAL
            .lock()
            .expect("test discovery serial lock poisoned");
        let live = spawn_current_session_daemon(&harnesses_dir().join("a-live"), "collected");
        let blocked = harnesses_dir().join("z-blocked");
        let listener =
            tau_socket::SocketListener::bind(socket_path(&blocked)).expect("blocked listener");
        let daemon = std::thread::spawn(move || {
            let _client = listener.accept().expect("accept final probe");
            std::thread::sleep(SESSION_DISCOVERY_PROBE_TIMEOUT);
        });
        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = Some((harnesses_dir(), 1_750));

        let error = list_running_sessions().expect_err("global probe deadline");

        *TEST_DISCOVERY_SCAN_DELAY
            .lock()
            .expect("test scan delay lock poisoned") = None;
        assert!(error.to_string().contains("probe deadline expired"));
        live.join().expect("live daemon");
        daemon.join().expect("blocked daemon");
    }

    /// Ensures session discovery ignores the old active-session value after a
    /// metadata rewrite and still finds the same live socket under the new id.
    #[test]
    fn find_harness_for_session_tracks_updated_active_session() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "old-session").expect("paths");
        let _listener = UnixListener::bind(paths.socket_path()).expect("socket");
        paths.write_metadata().expect("write metadata");

        update_session_id(paths.path(), "new-session").expect("update session");

        assert_eq!(
            find_harness_for_session("old-session").expect("old lookup"),
            None
        );
        assert_eq!(
            find_harness_for_session("new-session").expect("new lookup"),
            Some(paths.path().to_path_buf())
        );
    }

    /// A dead unreachable claimant is ignored but preserved because discovery
    /// cannot atomically exclude a concurrent PID-reuse/startup replacement.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_harness_for_session_preserves_dead_socket() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "session").expect("paths");
        write_metadata_with_pid(paths.path(), &project_root, "session", dead_pid());
        std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

        assert_eq!(find_harness_for_session("session").expect("lookup"), None);
        assert!(metadata_path(paths.path()).exists());
        assert!(paths.socket_path().exists());
    }

    /// Ensures a transient connection failure cannot unlink runtime metadata
    /// for a harness whose process still exists. This protects
    /// external-message discovery from turning one failed liveness probe
    /// into a permanent "no running daemon" failure for a live session.
    #[test]
    fn find_harness_for_session_keeps_files_when_pid_is_alive() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let child = ChildGuard::new();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "session").expect("paths");
        write_metadata_with_pid(paths.path(), &project_root, "session", child.id());
        std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

        assert!(matches!(
            find_harness_for_session("session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert!(metadata_path(paths.path()).exists());
        assert!(paths.socket_path().exists());
    }

    /// Hundreds of unrelated stale lifecycle pairs must not hide one live
    /// claimant merely because they exceed the general discovery candidate cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_harness_for_session_crosses_unrelated_stale_flood() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .expect("pid max")
            .trim()
            .parse()
            .expect("numeric pid max");
        for offset in 1..=300 {
            let pid = pid_max + offset;
            let path = dir.join(pid.to_string());
            drop(UnixListener::bind(socket_path(&path)).expect("stale socket"));
            write_metadata_with_pid(&path, temp.path(), "unrelated-session", pid);
        }
        let live = dir.join("live-target");
        let _listener = UnixListener::bind(socket_path(&live)).expect("live socket");
        write_peer_metadata(&live, "live-session", temp.path(), true);

        assert_eq!(
            find_harness_for_session("live-session").expect("bounded lookup"),
            Some(live)
        );
        assert_eq!(
            std::fs::read_dir(&dir).expect("runtime entries").count(),
            602,
            "lookup must remain non-destructive"
        );
    }

    /// More session-matching records than the candidate budget keep uniqueness
    /// incomplete even when one early claimant is live.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_harness_for_session_fails_closed_at_matching_candidate_bound() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let live = dir.join("live-target");
        let _listener = UnixListener::bind(socket_path(&live)).expect("live socket");
        write_peer_metadata(&live, "bounded-session", temp.path(), true);
        let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .expect("pid max")
            .trim()
            .parse()
            .expect("numeric pid max");
        for offset in 1..=SESSION_DISCOVERY_MAX_CANDIDATES as u32 {
            let pid = pid_max + offset;
            let path = dir.join(pid.to_string());
            drop(UnixListener::bind(socket_path(&path)).expect("stale socket"));
            write_metadata_with_pid(&path, temp.path(), "bounded-session", pid);
        }

        assert!(matches!(
            find_harness_for_session("bounded-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert_eq!(
            std::fs::read_dir(&dir).expect("runtime entries").count(),
            (SESSION_DISCOVERY_MAX_CANDIDATES + 1) * 2
        );
    }

    /// Cancellation that arrives after parsing the final unrelated metadata
    /// record must still prevent a false complete `None` result.
    #[test]
    fn find_harness_for_session_checks_cancellation_after_metadata_read() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let unrelated = dir.join("unrelated");
        write_peer_metadata(&unrelated, "other-session", temp.path(), true);
        TEST_CANCEL_AFTER_SESSION_METADATA_READ.with(|enabled| enabled.set(true));

        assert!(matches!(
            find_harness_for_session_until(
                "missing-session",
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
    }

    /// Project and session discovery are both non-destructive: a failed probe
    /// cannot destroy live daemon markers during a transient failure.
    #[test]
    fn find_harness_for_dir_keeps_files_when_pid_is_alive() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let child = ChildGuard::new();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "session").expect("paths");
        write_metadata_with_pid(paths.path(), &project_root, "session", child.id());
        std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

        assert_eq!(find_harness_for_dir(&project_root), None);
        assert!(metadata_path(paths.path()).exists());
        assert!(paths.socket_path().exists());
    }

    /// Ensures discovery reports ambiguity when two live harnesses advertise
    /// the same active session, preventing nondeterministic cross-harness
    /// sends.
    #[test]
    fn find_harness_for_session_reports_ambiguity() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let path_a = dir.join("a");
        let path_b = dir.join("b");
        let _listener_a = UnixListener::bind(socket_path(&path_a)).expect("socket a");
        let _listener_b = UnixListener::bind(socket_path(&path_b)).expect("socket b");
        for path in [&path_a, &path_b] {
            std::fs::write(
                metadata_path(path),
                serde_json::to_vec(&DaemonMetadata {
                    version: DAEMON_METADATA_VERSION,
                    pid: std::process::id(),
                    project_root: None,
                    session_id: "same-session".to_owned(),
                    peer_entrypoint: false,
                })
                .expect("metadata json"),
            )
            .expect("write metadata");
        }

        assert!(matches!(
            find_harness_for_session("same-session"),
            Err(FindHarnessForSessionError::Ambiguous { .. })
        ));
    }

    /// Once two live claimants are proven, later truncation, cancellation, or
    /// storage failure must retain the stronger true-ambiguity classification.
    #[test]
    fn incomplete_scan_preserves_proven_ambiguity() {
        let matches = vec![PathBuf::from("first"), PathBuf::from("second")];

        assert!(matches!(
            incomplete_or_ambiguous("same-session", &matches),
            FindHarnessForSessionError::Ambiguous {
                session_id,
                matches: classified,
            } if session_id == "same-session" && classified == matches
        ));
    }

    /// A lookup that exhausts the expanded raw-entry budget must not return a
    /// live claimant because an omitted entry could advertise the same session.
    #[test]
    fn find_harness_for_session_fails_closed_when_scan_is_incomplete() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        for index in 0..=SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES {
            std::fs::write(dir.join(format!("junk-{index:04}")), b"x").expect("junk entry");
        }
        let claimant = dir.join("claimant");
        let _listener = UnixListener::bind(socket_path(&claimant)).expect("claimant socket");
        write_peer_metadata(&claimant, "bounded-session", temp.path(), true);

        assert!(matches!(
            find_harness_for_session("bounded-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert!(socket_path(&claimant).exists());
        assert!(metadata_path(&claimant).exists());
    }

    /// A same-session record owned by a live process but temporarily
    /// unreachable keeps uniqueness unproven even when another claimant
    /// answers.
    #[test]
    fn find_harness_for_session_fails_closed_for_unreachable_live_claimant() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let reachable = dir.join("reachable");
        let _listener = UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
        let unreachable = dir.join("unreachable");
        drop(UnixListener::bind(socket_path(&unreachable)).expect("unreachable socket"));
        for path in [&reachable, &unreachable] {
            std::fs::write(
                metadata_path(path),
                serde_json::to_vec(&DaemonMetadata {
                    version: DAEMON_METADATA_VERSION,
                    pid: std::process::id(),
                    project_root: None,
                    session_id: "same-session".to_owned(),
                    peer_entrypoint: true,
                })
                .expect("metadata"),
            )
            .expect("write metadata");
        }

        assert!(matches!(
            find_harness_for_session("same-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
    }

    /// A partial live-PID metadata rewrite may hide a second claimant, so one
    /// valid live match is not enough to claim uniqueness.
    #[test]
    fn find_harness_for_session_fails_closed_for_malformed_live_metadata() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let reachable = dir.join("reachable");
        let _reachable_listener =
            UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
        write_peer_metadata(&reachable, "same-session", temp.path(), true);
        let unresolved = dir.join(std::process::id().to_string());
        let _unresolved_listener =
            UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
        std::fs::write(metadata_path(&unresolved), b"{\"session_id\":").expect("partial metadata");

        assert!(matches!(
            find_harness_for_session("same-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert!(socket_path(&unresolved).exists());
        assert!(metadata_path(&unresolved).exists());
    }

    /// A symlinked live-PID metadata record is never followed or ignored when
    /// doing so could hide a second session claimant.
    #[cfg(unix)]
    #[test]
    fn find_harness_for_session_fails_closed_for_symlinked_live_metadata() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let reachable = dir.join("reachable");
        let _reachable_listener =
            UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
        write_peer_metadata(&reachable, "same-session", temp.path(), true);
        let unresolved = dir.join(std::process::id().to_string());
        let _unresolved_listener =
            UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
        let target = temp.path().join("metadata-target");
        std::fs::write(&target, b"{}").expect("symlink target");
        std::os::unix::fs::symlink(&target, metadata_path(&unresolved)).expect("symlink metadata");

        assert!(matches!(
            find_harness_for_session("same-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert!(socket_path(&unresolved).exists());
        assert!(metadata_path(&unresolved).is_symlink());
    }

    /// Oversized live-PID metadata remains unresolved without reading beyond
    /// the byte bound or returning an early live match as unique.
    #[test]
    fn find_harness_for_session_fails_closed_for_oversized_live_metadata() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let dir = harnesses_dir();
        std::fs::create_dir_all(&dir).expect("harnesses dir");
        let reachable = dir.join("reachable");
        let _reachable_listener =
            UnixListener::bind(socket_path(&reachable)).expect("reachable socket");
        write_peer_metadata(&reachable, "same-session", temp.path(), true);
        let unresolved = dir.join(std::process::id().to_string());
        let _unresolved_listener =
            UnixListener::bind(socket_path(&unresolved)).expect("unresolved socket");
        std::fs::write(
            metadata_path(&unresolved),
            vec![b' '; SESSION_DISCOVERY_MAX_METADATA_BYTES as usize + 1],
        )
        .expect("oversized metadata");

        assert!(matches!(
            find_harness_for_session("same-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
        assert_eq!(
            std::fs::metadata(metadata_path(&unresolved))
                .expect("metadata remains")
                .len(),
            SESSION_DISCOVERY_MAX_METADATA_BYTES + 1
        );
    }

    /// Failure to enumerate an existing runtime catalog is incomplete rather
    /// than evidence that no matching daemon exists.
    #[test]
    fn find_harness_for_session_fails_closed_when_catalog_is_not_a_directory() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        std::fs::create_dir_all(root_runtime_dir()).expect("runtime root");
        std::fs::write(harnesses_dir(), b"not a directory").expect("catalog marker");

        assert!(matches!(
            find_harness_for_session("missing-session"),
            Err(FindHarnessForSessionError::Incomplete { .. })
        ));
    }

    /// Ensures daemon startup creates private runtime directories rather than
    /// relying on the process umask, protecting fallback IPC sockets and
    /// metadata from other local users.
    #[cfg(unix)]
    #[test]
    fn prepare_harness_paths_creates_private_runtime_dirs() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");

        let paths = prepare_harness_paths(&project_root, "session").expect("paths");

        let root_mode = std::fs::metadata(root_runtime_dir())
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777;
        let harnesses_mode = std::fs::metadata(harnesses_dir())
            .expect("harnesses metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
        assert_eq!(harnesses_mode, 0o700);
        assert_eq!(paths.path().parent(), Some(harnesses_dir().as_path()));
    }

    /// Ensures a pre-existing runtime symlink is refused instead of followed
    /// before binding daemon IPC sockets or writing discovery metadata.
    #[cfg(unix)]
    #[test]
    fn prepare_harness_paths_rejects_symlink_runtime_root() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).expect("real runtime target");
        std::os::unix::fs::symlink(&real, root_runtime_dir()).expect("runtime symlink");
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");

        let error = match prepare_harness_paths(&project_root, "session") {
            Ok(_) => panic!("symlink runtime root must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("must not be a symlink"));
    }

    fn write_metadata_with_pid(path: &Path, project_root: &Path, session_id: &str, pid: u32) {
        std::fs::write(
            metadata_path(path),
            serde_json::to_vec(&DaemonMetadata {
                version: DAEMON_METADATA_VERSION,
                pid,
                project_root: Some(project_root.to_path_buf()),
                session_id: session_id.to_owned(),
                peer_entrypoint: false,
            })
            .expect("metadata json"),
        )
        .expect("write metadata");
    }

    struct ChildGuard {
        child: Child,
    }

    impl ChildGuard {
        fn new() -> Self {
            Self {
                child: Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .expect("spawn sleep"),
            }
        }

        fn id(&self) -> u32 {
            self.child.id()
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(target_os = "linux")]
    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        pid
    }
}
