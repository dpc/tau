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
    /// Session id bound to the daemon at startup.
    pub session_id: String,
}

/// Returns the root runtime directory for all tau daemon instances.
#[must_use]
pub fn root_runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .map(|dir| dir.join("tau"))
        .unwrap_or_else(|| {
            let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
            PathBuf::from(format!("/tmp/tau-{user}"))
        })
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

/// Creates paths and metadata for the current process.
pub fn prepare_harness_paths(
    project_root: &Path,
    session_id: &str,
) -> Result<HarnessPaths, std::io::Error> {
    let pid = std::process::id();
    let path = harnesses_dir().join(pid.to_string());
    std::fs::create_dir_all(harnesses_dir())?;
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

/// Finds a running harness daemon for the given project root.
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
            } else {
                remove_harness_files(&harness_path);
            }
        }
    }

    None
}

/// Verifies that a daemon is actually running by connecting to its
/// socket.
fn verify_harness_running(harness_path: &Path) -> bool {
    UnixStream::connect(socket_path(harness_path)).is_ok()
}

/// Removes the socket and metadata for a stale harness path stem.
pub fn remove_harness_files(harness_path: &Path) {
    let _ = std::fs::remove_file(socket_path(harness_path));
    let _ = std::fs::remove_file(metadata_path(harness_path));
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a_canon), Ok(b_canon)) => a_canon == b_canon,
        _ => a == b,
    }
}
