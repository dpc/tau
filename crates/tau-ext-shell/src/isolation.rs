//! Child-process isolation for shell-style commands.
//!
//! Used for every external command this crate spawns so the agent's
//! commands are detached from the harness's tty and don't hang on
//! interactive stdin.

#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::io as path_std_io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

#[cfg(target_os = "linux")]
use tracing::warn;

const CARGO_BUILD_ENV_VARS: &[&str] = &[
    "CARGO",
    "CARGO_BIN_NAME",
    "CARGO_CRATE_NAME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_LINKS",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PRIMARY_PACKAGE",
    "OUT_DIR",
];

/// Sanitize a `Command` so the child is fully detached from the
/// harness's controlling terminal:
///
/// - Overrides color-request environment variables with `NO_COLOR=1` /
///   `CLICOLOR=0` while preserving `TERM` so output PTYs retain useful terminal
///   formatting.
/// - Clears Cargo build-time variables that can confuse tools executed outside
///   the extension's build context.
/// - The spawn layer closes stdin on every platform, so interactive prompts
///   fail fast.
/// - On Unix, runs `setsid()` in the child so it becomes the leader of a new
///   session with no controlling terminal — even an explicit `open("/dev/tty")`
///   will fail rather than reach the harness's tty.
pub(crate) fn apply_command_isolation(cmd: &mut Command) {
    cmd.env("NO_COLOR", "1").env("CLICOLOR", "0");

    // Work around Cargo leaking build-time environment into commands run during
    // development, which can make nested Cargo invocations rebuild needlessly:
    // https://github.com/rust-lang/cargo/issues/16134
    for env_var in CARGO_BUILD_ENV_VARS {
        cmd.env_remove(env_var);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and only mutates the
        // calling (child) process's session/pgid — no allocator, no
        // locks, no shared state with the parent.
        //
        // Failure inside `pre_exec` aborts the spawn, so be strict
        // about what we treat as a failure: `EPERM` means the child
        // is already a session leader, which is exactly the state we
        // were trying to reach — silently accept it.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(apply_session_isolation);
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn apply_session_isolation() -> std::io::Result<()> {
    if unsafe { libc::setsid() } == -1 {
        let err = path_std_io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EPERM) {
            return Err(err);
        }
    }
    Ok(())
}

/// Add child-side mount namespace setup that bind mounts `cwd` read-only.
///
/// The command still sees the rest of the filesystem as before, but the working
/// directory subtree is over-mounted read-only before the shell is exec'd.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn apply_read_only_cwd_mount(
    cmd: &mut Command,
    cwd: &std::path::Path,
) -> std::io::Result<Option<ReadOnlyMountWarningPipe>> {
    use std::os::unix::process::CommandExt;

    let cwd = cwd.canonicalize()?;
    let cwd = CString::new(cwd.as_os_str().as_bytes())?;
    #[allow(unsafe_code)]
    let uid = unsafe { libc::geteuid() };
    #[allow(unsafe_code)]
    let gid = unsafe { libc::getegid() };
    let uid_map = CString::new(format!("0 {uid} 1\n"))?;
    let gid_map = CString::new(format!("0 {gid} 1\n"))?;

    let mut pipe_fds = [0; 2];
    cvt(unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let read_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
    let warning_write_fd = write_fd.as_raw_fd();

    // SAFETY: the closure runs in the forked child before exec. It only calls
    // libc syscalls and uses CStrings allocated before fork, avoiding allocator
    // or lock interaction in the child.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            apply_read_only_cwd_mount_child(&cwd, &uid_map, &gid_map, warning_write_fd)
        });
    }
    Ok(Some(ReadOnlyMountWarningPipe { read_fd, write_fd }))
}

#[cfg(target_os = "linux")]
pub(crate) struct ReadOnlyMountWarningPipe {
    read_fd: OwnedFd,
    write_fd: OwnedFd,
}

#[cfg(target_os = "linux")]
impl ReadOnlyMountWarningPipe {
    #[allow(unsafe_code)]
    pub(crate) fn log_after_spawn(self) {
        drop(self.write_fd);
        let mut buf = [0u8; 128];
        let len = unsafe {
            libc::read(
                self.read_fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if 0 < len {
            let message = String::from_utf8_lossy(&buf[..len as usize]);
            warn!(reason = %message.trim(), "shell read-only cwd mount unavailable");
        }
    }
}

/// Non-Linux platforms have no Linux bind mounts to apply.
#[cfg(not(target_os = "linux"))]
pub(crate) fn apply_read_only_cwd_mount(
    _cmd: &mut Command,
    _cwd: &std::path::Path,
) -> std::io::Result<Option<ReadOnlyMountWarningPipe>> {
    Err(path_std_io::Error::new(
        path_std_io::ErrorKind::Unsupported,
        "read-only cwd mount is only supported on Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct ReadOnlyMountWarningPipe;

#[cfg(not(target_os = "linux"))]
impl ReadOnlyMountWarningPipe {
    pub(crate) fn log_after_spawn(self) {}
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn apply_read_only_cwd_mount_child(
    cwd: &CStr,
    uid_map: &CStr,
    gid_map: &CStr,
    warning_write_fd: libc::c_int,
) -> std::io::Result<()> {
    if cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) }).is_err() {
        let _ = warning_write_fd;
        return Err(path_std_io::Error::last_os_error());
    }
    write_proc_file(c"/proc/self/setgroups", c"deny\n", true)?;
    write_proc_file(c"/proc/self/uid_map", uid_map, false)?;
    write_proc_file(c"/proc/self/gid_map", gid_map, false)?;
    cvt(unsafe { libc::setresgid(0, 0, 0) })?;
    cvt(unsafe { libc::setresuid(0, 0, 0) })?;

    cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    cvt(unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })?;
    if let Err(error) = bind_mount_read_only(cwd) {
        write_warning(warning_write_fd, b"read-only bind mount failed");
        return Err(error);
    }
    cvt(unsafe { libc::chdir(cwd.as_ptr()) })?;
    drop_namespace_capabilities()?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn bind_mount_read_only(cwd: &CStr) -> std::io::Result<()> {
    let detached_fd = cvt_fd(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            cwd.as_ptr(),
            libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | (libc::AT_RECURSIVE as libc::c_uint),
        )
    })?;
    let move_result = cvt_long(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            detached_fd,
            c"".as_ptr(),
            libc::AT_FDCWD,
            cwd.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    });
    let close_detached = unsafe { libc::close(detached_fd) };
    move_result?;
    cvt(close_detached)?;

    let mounted_fd = cvt_fd(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            cwd.as_ptr(),
            libc::OPEN_TREE_CLOEXEC,
        )
    })?;
    let attr = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    let setattr_result = cvt_long(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            mounted_fd,
            c"".as_ptr(),
            read_only_mount_setattr_flags(),
            (&attr as *const libc::mount_attr).cast::<libc::c_void>(),
            std::mem::size_of::<libc::mount_attr>(),
        )
    });
    let close_mounted = unsafe { libc::close(mounted_fd) };
    setattr_result?;
    cvt(close_mounted)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn read_only_mount_setattr_flags() -> libc::c_uint {
    (libc::AT_EMPTY_PATH | libc::AT_RECURSIVE) as libc::c_uint
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn drop_namespace_capabilities() -> std::io::Result<()> {
    let securebits = libc::SECBIT_NOROOT
        | libc::SECBIT_NOROOT_LOCKED
        | libc::SECBIT_NO_SETUID_FIXUP
        | libc::SECBIT_NO_SETUID_FIXUP_LOCKED
        | libc::SECBIT_NO_CAP_AMBIENT_RAISE
        | libc::SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
    cvt(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, securebits, 0, 0, 0) })?;
    cvt(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            (&mut header as *mut CapHeader).cast::<libc::c_void>(),
            data.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result == -1 {
        Err(path_std_io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn write_warning(fd: libc::c_int, message: &[u8]) {
    unsafe {
        let _ = libc::write(fd, message.as_ptr().cast::<libc::c_void>(), message.len());
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn write_proc_file(path: &CStr, content: &CStr, ignore_enoent: bool) -> std::io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd == -1 {
        let err = path_std_io::Error::last_os_error();
        if ignore_enoent && err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(err);
    }

    let bytes = content.to_bytes();
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
    let close_result = unsafe { libc::close(fd) };
    if written < 0 {
        return Err(path_std_io::Error::last_os_error());
    }
    if written as usize != bytes.len() {
        return Err(path_std_io::Error::new(
            path_std_io::ErrorKind::WriteZero,
            "short write to proc uid/gid map",
        ));
    }
    cvt(close_result)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn cvt(result: libc::c_int) -> std::io::Result<()> {
    if result == -1 {
        Err(path_std_io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn cvt_long(result: libc::c_long) -> std::io::Result<()> {
    if result == -1 {
        Err(path_std_io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn cvt_fd(result: libc::c_long) -> std::io::Result<libc::c_int> {
    if result == -1 {
        Err(path_std_io::Error::last_os_error())
    } else {
        Ok(result as libc::c_int)
    }
}
