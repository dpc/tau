//! Pseudo-terminal attachment for shell output on explicitly supported targets.

use std::process::{Command, Stdio};

use rustix::fd::OwnedFd;
use rustix::termios::{OptionalActions, OutputModes, Winsize, tcgetattr, tcsetattr};
#[cfg(any(target_os = "android", target_os = "linux"))]
use rustix_openpty::rustix;
#[cfg(target_os = "macos")]
use rustix_v1 as rustix;

/// The controller and user endpoints for one output PTY.
struct Pty {
    /// Parent-side endpoint used to capture output.
    controller: OwnedFd,
    /// Child-side endpoint attached to one output stream.
    user: OwnedFd,
}

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
    /// Each platform allocator creates PTY endpoints atomically close-on-exec.
    /// `OwnedFd::try_clone` uses atomic `F_DUPFD_CLOEXEC`, so every parent-only
    /// endpoint retains that inheritance invariant.
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
#[cfg(any(target_os = "android", target_os = "linux"))]
fn open_pty() -> std::io::Result<Pty> {
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
    Ok(Pty {
        controller: pty.controller,
        user: pty.user,
    })
}

/// Open one macOS PTY with atomically close-on-exec endpoints.
#[cfg(target_os = "macos")]
fn open_pty() -> std::io::Result<Pty> {
    use rustix::fs::{Mode, OFlags, open};
    use rustix::pty::{grantpt, ptsname, unlockpt};
    use rustix::termios::tcsetwinsize;

    let flags = OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC;
    let controller = open("/dev/ptmx", flags, Mode::empty())?;
    grantpt(&controller)?;
    unlockpt(&controller)?;
    let user_name = ptsname(&controller, Vec::new())?;
    let user = open(user_name.as_c_str(), flags, Mode::empty())?;
    tcsetwinsize(
        &user,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    configure_user(&user)?;
    Ok(Pty { controller, user })
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
