//! Linux and Android pseudo-terminal attachment for shell output descriptors.

use std::process::{Command, Stdio};

use rustix_openpty::rustix::fd::OwnedFd;
use rustix_openpty::rustix::termios::{
    OptionalActions, OutputModes, Winsize, tcgetattr, tcsetattr,
};

/// Parent-side output PTY resources attached to one shell command.
pub(crate) struct PtyStdio {
    /// Stdout controller read by ext-shell capture.
    pub(crate) stdout_controller: std::fs::File,
    /// Stderr controller read by ext-shell capture.
    pub(crate) stderr_controller: std::fs::File,
    /// Output user-side guards retained until foreground process completion.
    pub(crate) output_users: [std::fs::File; 2],
}

impl PtyStdio {
    /// Open independent PTYs and attach their user sides to a command.
    ///
    /// `rustix-openpty` creates CLOEXEC endpoints atomically on Linux and
    /// Android. `OwnedFd::try_clone` uses atomic `F_DUPFD_CLOEXEC`, so every
    /// parent-only endpoint retains that inheritance invariant.
    pub(crate) fn attach(command: &mut Command) -> std::io::Result<Self> {
        let stdout = open_pty()?;
        let stderr = open_pty()?;
        let stdout_user_guard = stdout.user.try_clone()?;
        let stderr_user_guard = stderr.user.try_clone()?;

        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.user))
            .stderr(Stdio::from(stderr.user));

        Ok(Self {
            stdout_controller: std::fs::File::from(stdout.controller),
            stderr_controller: std::fs::File::from(stderr.controller),
            output_users: [
                std::fs::File::from(stdout_user_guard),
                std::fs::File::from(stderr_user_guard),
            ],
        })
    }
}

/// Open one 24x80 output PTY with byte-preserving terminal behavior.
fn open_pty() -> std::io::Result<rustix_openpty::Pty> {
    let pty = rustix_openpty::openpty(
        None,
        Some(&Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
    )?;
    configure_user(&pty.user)?;
    Ok(pty)
}

/// Configure one user endpoint without unsafe descriptor manipulation.
fn configure_user(user: &OwnedFd) -> std::io::Result<()> {
    let mut attributes = tcgetattr(user)?;
    attributes.output_modes.remove(OutputModes::OPOST);
    tcsetattr(user, OptionalActions::Now, &attributes)?;
    Ok(())
}

#[cfg(test)]
mod tests;
