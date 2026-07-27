//! Spawned shell process and standard-stream endpoint ownership.

use std::process::Command;

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
use crate::pty_stdio::PtyStdio;

/// A spawned shell process and its separately captured output streams.
pub(crate) struct ShellProcess {
    /// Foreground shell child transferred to the platform lifecycle
    /// implementation.
    pub(crate) child: std::process::Child,
    /// Captured stdout, backed by a PTY controller on supported PTY targets.
    pub(crate) stdout: Option<ShellStdout>,
    /// Captured stderr, backed by a separate PTY controller on supported PTY
    /// targets.
    pub(crate) stderr: Option<ShellStderr>,
    /// User-side guards preventing pre-exec output-controller hangups.
    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    pub(crate) output_users: Option<[std::fs::File; 2]>,
}

/// Platform-specific readable endpoint used to capture shell standard output.
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
pub(crate) type ShellStdout = std::fs::File;
/// Platform-specific readable endpoint used to capture shell standard error.
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
pub(crate) type ShellStderr = std::fs::File;
/// Platform-specific readable endpoint used to capture shell standard output.
#[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
pub(crate) type ShellStdout = std::process::ChildStdout;
/// Platform-specific readable endpoint used to capture shell standard error.
#[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
pub(crate) type ShellStderr = std::process::ChildStderr;

impl ShellProcess {
    /// Attach platform standard streams and spawn one shell command.
    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        let ptys = PtyStdio::attach(command)?;
        let child = command.spawn()?;
        Ok(Self {
            child,
            stdout: Some(ptys.stdout_controller),
            stderr: Some(ptys.stderr_controller),
            output_users: Some(ptys.output_users),
        })
    }

    /// Attach closed stdin plus output pipes on platforms without
    /// atomic-CLOEXEC PTY support.
    ///
    /// A create-then-`fcntl` PTY setup could leak a parent-only endpoint across
    /// a concurrent spawn, so these targets deliberately retain pipe
    /// capture.
    #[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos")))]
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }
}
