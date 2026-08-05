//! Shared isolated process fixtures for Tau CLI integration tests.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

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
    let runtime = root.join(".runtime");
    for path in [&work, &config, &state, &cache, &data, &runtime] {
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
