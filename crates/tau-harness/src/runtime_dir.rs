//! Daemon runtime directory management.
//!
//! Each discoverable harness daemon gets one socket plus one adjacent metadata
//! file under `$XDG_RUNTIME_DIR/tau/harnesses/`:
//!
//! - `<pid>-<instance>.sock` — Unix socket for client connections
//! - `<pid>-<instance>.json` — daemon metadata used for discovery
//!
//! Metadata-based discovery enumerates `*.sock`, reads matching `*.json`, then
//! verifies liveness. Running-session listing instead treats sockets as
//! candidates and asks each responsive harness for its in-memory session
//! identity.

#[cfg(test)]
mod tests;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const HARNESSES_DIR: &str = "harnesses";
const SOCK_EXTENSION: &str = "sock";
const METADATA_EXTENSION: &str = "json";
const HARNESS_INSTANCE_ID_HEX_LEN: usize = 16;
const SESSION_DISCOVERY_MAX_CANDIDATES: usize = 128;
const SESSION_LOOKUP_MAX_DIRECTORY_ENTRIES: usize = 4_096;
const SESSION_DISCOVERY_MAX_METADATA_BYTES: u64 = 16 * 1024;
const SESSION_DISCOVERY_MAX_PROBES: usize = 8;
const SESSION_DISCOVERY_MAX_CALLS: usize = 8;
const SESSION_DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const SESSION_DISCOVERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(2);
// Keep at zero per `GATE-no-backward-compatibility`.
const DAEMON_METADATA_VERSION: u32 = 0;

/// Maximum number of sessions returned by one discovery request.
pub const SESSION_DISCOVERY_MAX_RESULTS: usize = 50;

/// Private CLI-to-harness transport for the minted runtime instance id.
pub const HARNESS_INSTANCE_ID_ENV: &str = "TAU_HARNESS_INSTANCE_ID";

static ACTIVE_DISCOVERY_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Random process-instance discriminator used in one daemon runtime path.
///
/// The process id remains in the path for diagnostics, while this discriminator
/// prevents unrelated PID namespaces that share an XDG runtime directory from
/// selecting the same socket pathname.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessInstanceId {
    /// Fixed-width lowercase hexadecimal discriminator.
    value: String,
}

impl HarnessInstanceId {
    /// Mints a new random runtime instance discriminator.
    #[must_use]
    pub fn mint() -> Self {
        Self {
            value: format!("{:016x}", rand::random::<u64>()),
        }
    }

    /// Parses the private instance-id transport supplied by a spawning CLI.
    ///
    /// # Errors
    ///
    /// Returns invalid input when the value is not exactly 16 lowercase
    /// hexadecimal ASCII characters.
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, std::io::Error> {
        let value = value.into();
        if value.len() != HARNESS_INSTANCE_ID_HEX_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid harness runtime instance id",
            ));
        }
        Ok(Self { value })
    }

    /// Returns the serialized instance discriminator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

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
    /// metadata compatibility. Harnesses that support `:session new` update it
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

/// Returns the runtime path stem for one PID and process instance.
#[must_use]
pub fn harness_path_for_process(pid: u32, instance_id: &HarnessInstanceId) -> PathBuf {
    harnesses_dir().join(format!("{pid}-{}", instance_id.as_str()))
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
    let Ok(session_id) = tau_proto::SessionId::parse(session_id) else {
        return false;
    };
    let ProbeConnect::Connected(mut peer, probe_deadline) = connect_probe_peer(
        harness_path,
        deadline,
        cancelled,
        tau_proto::ExtensionName::parse(crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME)
            .expect("built-in extension name must satisfy the extension identifier grammar"),
        tau_proto::ClientKind::External,
    ) else {
        return false;
    };
    let request_id = format!("peer-probe-{}", std::process::id());
    if peer
        .send(&tau_proto::HarnessInputMessage::PeerSessionProbe(
            tau_proto::PeerSessionProbe {
                request_id: request_id.clone(),
                session_id,
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

/// Reads the validated session id a running daemon at `harness_path` is bound
/// to.
///
/// Reports missing metadata, malformed JSON, and invalid controlled identifiers
/// as distinct I/O errors.
pub fn read_session_id(harness_path: &Path) -> Result<tau_proto::SessionId, std::io::Error> {
    let encoded = std::fs::read_to_string(metadata_path(harness_path))?;
    let metadata: DaemonMetadata = serde_json::from_str(&encoded).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed daemon metadata: {error}"),
        )
    })?;
    tau_proto::SessionId::parse(metadata.session_id).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid daemon session id: {error}"),
        )
    })
}

/// Updates the active/current session id in an existing daemon metadata file.
pub fn update_session_id(harness_path: &Path, session_id: &str) -> Result<(), std::io::Error> {
    let session_id = validate_session_id_for_metadata(session_id)?;
    let mut metadata = read_metadata(harness_path)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "metadata missing"))?;
    metadata.session_id = session_id.to_string();
    let json = serde_json::to_vec_pretty(&metadata).map_err(std::io::Error::other)?;
    std::fs::write(metadata_path(harness_path), json)
}

/// Creates paths and metadata for the current process.
pub fn prepare_harness_paths(
    project_root: &Path,
    session_id: &str,
) -> Result<HarnessPaths, std::io::Error> {
    validate_session_id_for_metadata(session_id)?;
    prepare_harness_paths_for_instance(project_root, session_id, &HarnessInstanceId::mint())
}

/// Creates paths and metadata for one explicitly identified process instance.
pub(crate) fn prepare_harness_paths_for_instance(
    project_root: &Path,
    session_id: &str,
    instance_id: &HarnessInstanceId,
) -> Result<HarnessPaths, std::io::Error> {
    validate_session_id_for_metadata(session_id)?;
    let pid = std::process::id();
    let root_dir = root_runtime_dir();
    ensure_private_runtime_dir(&root_dir)?;
    let harnesses_dir = root_dir.join(HARNESSES_DIR);
    ensure_private_runtime_dir(&harnesses_dir)?;
    let path = harness_path_for_process(pid, instance_id);
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

fn validate_session_id_for_metadata(
    session_id: &str,
) -> Result<tau_proto::SessionId, std::io::Error> {
    tau_proto::SessionId::parse(session_id).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid session id `{session_id}`: {error}"),
        )
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
            tau_proto::ExtensionName::parse("tau-session-list")
                .expect("built-in extension name must satisfy the extension identifier grammar"),
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
                    if harness_stem_liveness(&harness_path)
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
            if SESSION_DISCOVERY_MAX_CANDIDATES < matching_candidates {
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

/// Returns liveness only for a conventional PID-prefixed daemon path stem.
fn harness_stem_liveness(harness_path: &Path) -> Option<ProcessLiveness> {
    harness_path
        .file_name()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| {
            stem.split_once('-')
                .map_or(stem, |(pid, _)| pid)
                .parse::<u32>()
                .ok()
        })
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
