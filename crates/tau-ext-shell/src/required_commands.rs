//! Startup diagnostics for fixed executables invoked by extension-owned tools.

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::path::Path;

use tau_proto::{ExtensionNoticeRequest, HarnessInputMessage, NoticeLevel};

/// Fixed external executables used by extension-owned tools, not user shell
/// commands.
const REQUIRED_COMMANDS: &[(&str, &str)] = &[("rg", "grep")];

/// Checks the process PATH used by extension-owned subprocesses at startup.
pub(super) fn missing_command_notices() -> Vec<HarnessInputMessage> {
    missing_command_notices_on_path(std::env::var_os("PATH").as_deref())
}

fn missing_command_notices_on_path(path: Option<&OsStr>) -> Vec<HarnessInputMessage> {
    REQUIRED_COMMANDS
        .iter()
        .filter(|(command, _)| !available_on_path(command, path))
        .map(|(command, tool)| {
            HarnessInputMessage::ExtensionNoticeRequest(ExtensionNoticeRequest {
                message: format!(
                    "shell extension: required command `{command}` was not found on PATH; \
                     install it or add it to the extension's PATH to use the `{tool}` tool"
                ),
                level: NoticeLevel::Warning,
            })
        })
        .collect()
}

fn available_on_path(command: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|directory| executable(&directory.join(command)))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
