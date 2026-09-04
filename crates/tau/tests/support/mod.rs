//! Shared isolated process fixtures for Tau CLI integration tests.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::Permissions;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIX_SOCKET_PATH_BYTES: usize = 107;
const SESSION_SOCKET_SUFFIX_BYTES: usize = 1 + 3 + 1 + 9 + 1 + 7 + 1 + 64 + 5;

/// Creates a private runtime root whose longest fixed-session socket path fits
/// Linux `sockaddr_un::sun_path`.
pub fn bounded_runtime_tempdir() -> tempfile::TempDir {
    let configured = tempfile::env::temp_dir();
    for parent in [
        Path::new("/tmp"),
        Path::new("/dev/shm"),
        configured.parent().unwrap_or(configured.as_path()),
    ] {
        if let Ok(tempdir) = tempfile::Builder::new().prefix("t").tempdir_in(parent)
            && tempdir.path().as_os_str().as_encoded_bytes().len() + SESSION_SOCKET_SUFFIX_BYTES
                <= UNIX_SOCKET_PATH_BYTES
        {
            return tempdir;
        }
    }
    panic!("no writable temporary parent yields a bounded Tau runtime socket path");
}

/// Creates a Tau command with only private home, XDG, runtime, and cwd inputs.
///
/// Callers add the one environment variable their assertion needs after this
/// function returns. Starting from an empty environment keeps unrelated Tau
/// profile, CLI-transport, and secret inputs out of subprocess startup.
pub fn isolated_tau_command(program: impl AsRef<OsStr>, root: &Path) -> Command {
    let work = root.join("work");
    let config = root.join(".config");
    let state = root.join(".state");
    let cache = root.join(".cache");
    let data = root.join(".data");
    let runtime = isolated_runtime_dir(root);
    for path in [&work, &config, &state, &cache, &data] {
        std::fs::create_dir_all(path).expect("create isolated Tau process root");
    }

    let mut command = Command::new(program);
    command
        .env_clear()
        .current_dir(work)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_DATA_HOME", data)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("LANG", "C.UTF-8")
        .env("TERM", "xterm-256color");
    command
}

/// Returns a deterministic short runtime root for one isolated test root.
///
/// Nix builds place test roots below `/build`, where appending Tau's fixed
/// 64-byte session key would exceed Linux's Unix-socket path limit.
pub fn isolated_runtime_dir(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let runtime = PathBuf::from(format!("/tmp/t{:08x}", hasher.finish() as u32));
    std::fs::create_dir_all(&runtime).expect("create short isolated runtime root");
    std::fs::set_permissions(&runtime, Permissions::from_mode(0o700))
        .expect("set short isolated runtime permissions");
    runtime
}
