//! Daemon runtime directory management.
//!
//! Each discoverable harness daemon gets one socket plus one adjacent metadata
//! file under `$XDG_RUNTIME_DIR/tau/harnesses/`:
//!
//! - `<pid>.sock` — Unix socket for client connections
//! - `<pid>.json` — daemon metadata used for discovery
//!
//! Discovery is socket-first: clients enumerate `*.sock`, read the matching
//! `*.json`, then verify liveness by connecting to the socket.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const HARNESSES_DIR: &str = "harnesses";
const SOCK_EXTENSION: &str = "sock";
const METADATA_EXTENSION: &str = "json";

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
}

/// Error returned when session-based daemon discovery is ambiguous.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FindHarnessForSessionError {
    /// More than one live harness advertises the requested active session.
    Ambiguous {
        session_id: String,
        matches: Vec<PathBuf>,
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

#[cfg(test)]
thread_local! {
    static TEST_RUNTIME_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
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
            version: 1,
            pid,
            project_root: Some(project_root.to_path_buf()),
            session_id: session_id.to_owned(),
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
/// Discovery verifies matching candidates by connecting to their sockets. If a
/// matching socket cannot be reached, its runtime files are removed only when
/// the metadata pid is known to be no longer running; live-pid files are
/// preserved so a transient liveness-probe failure cannot make a daemon
/// permanently undiscoverable. Platforms without a safe pid-liveness backend
/// conservatively preserve unreachable candidates.
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
        {
            if verify_harness_running(&harness_path) {
                return Some(harness_path);
            } else if !is_process_running(metadata.pid) {
                remove_harness_files(&harness_path);
            }
        }
    }

    None
}

/// Finds the single live harness advertising `session_id` as its active
/// session.
///
/// Discovery verifies candidates by connecting to their sockets. If a matching
/// socket cannot be reached, its runtime files are removed only when the
/// metadata pid is known to be no longer running. When the pid is still alive
/// or cannot be safely checked, discovery returns no match for this call but
/// preserves the files so a transient liveness-probe failure does not make a
/// live daemon permanently undiscoverable. Returns `Ok(None)` when no live
/// daemon advertises the session and `Err` when multiple live daemons do.
pub fn find_harness_for_session(
    session_id: &str,
) -> Result<Option<PathBuf>, FindHarnessForSessionError> {
    let runtime_dir = harnesses_dir();
    if !runtime_dir.exists() {
        return Ok(None);
    }

    let mut matches = Vec::new();
    let Ok(entries) = std::fs::read_dir(&runtime_dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let sock = entry.path();
        if sock.extension().and_then(|ext| ext.to_str()) != Some(SOCK_EXTENSION) {
            continue;
        }
        let harness_path = sock.with_extension("");
        let Some(metadata) = read_metadata(&harness_path) else {
            continue;
        };
        if metadata.session_id != session_id {
            continue;
        }
        if verify_harness_running(&harness_path) {
            matches.push(harness_path);
        } else if !is_process_running(metadata.pid) {
            remove_harness_files(&harness_path);
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(FindHarnessForSessionError::Ambiguous {
            session_id: session_id.to_owned(),
            matches,
        }),
    }
}

/// Verifies that a daemon is actually running by connecting to its
/// socket.
pub(crate) fn verify_harness_running(harness_path: &Path) -> bool {
    UnixStream::connect(socket_path(harness_path)).is_ok()
}

/// Removes the socket and metadata for a stale harness path stem.
pub fn remove_harness_files(harness_path: &Path) {
    let _ = std::fs::remove_file(socket_path(harness_path));
    let _ = std::fs::remove_file(metadata_path(harness_path));
}

#[cfg(target_os = "linux")]
fn is_process_running(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn is_process_running(_pid: u32) -> bool {
    // Tau forbids unsafe code, so avoid libc `kill(pid, 0)` here. Preserving
    // files is the conservative fallback: discovery still never returns a
    // candidate whose socket probe failed, but non-Linux platforms may retain
    // stale runtime files until a future safe liveness backend is added.
    true
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

    struct RuntimeOverride;

    impl Drop for RuntimeOverride {
        fn drop(&mut self) {
            TEST_RUNTIME_DIR.with(|dir| *dir.borrow_mut() = None);
        }
    }

    fn runtime_override(temp: &TempDir) -> RuntimeOverride {
        TEST_RUNTIME_DIR.with(|dir| *dir.borrow_mut() = Some(temp.path().to_path_buf()));
        RuntimeOverride
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

    /// Ensures session discovery removes dead socket metadata instead of
    /// returning a daemon that cannot accept the external-message RPC.
    #[cfg(target_os = "linux")]
    #[test]
    fn find_harness_for_session_removes_stale_socket() {
        let temp = TempDir::new().expect("temp runtime");
        let _guard = runtime_override(&temp);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).expect("project root");
        let paths = prepare_harness_paths(&project_root, "session").expect("paths");
        write_metadata_with_pid(paths.path(), &project_root, "session", dead_pid());
        std::fs::write(paths.socket_path(), b"not a listener").expect("stale socket marker");

        assert_eq!(find_harness_for_session("session").expect("lookup"), None);
        assert!(!metadata_path(paths.path()).exists());
        assert!(!paths.socket_path().exists());
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

        assert_eq!(find_harness_for_session("session").expect("lookup"), None);
        assert!(metadata_path(paths.path()).exists());
        assert!(paths.socket_path().exists());
    }

    /// Ensures project-root discovery uses the same pid-gated stale cleanup as
    /// session discovery. External-message sends use session lookup, while CLI
    /// attach uses project lookup; both must avoid destroying live daemon
    /// markers after a transient probe failure.
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
                    version: 1,
                    pid: std::process::id(),
                    project_root: None,
                    session_id: "same-session".to_owned(),
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
                version: 1,
                pid,
                project_root: Some(project_root.to_path_buf()),
                session_id: session_id.to_owned(),
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
