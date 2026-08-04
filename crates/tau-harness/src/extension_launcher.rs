//! Fail-closed Linux namespace setup for supervised extensions.
//!
//! The hook deliberately performs all allocation, formatting, path conversion,
//! and filesystem discovery before `fork`. Between `fork` and `exec` it invokes
//! only direct libc system-call wrappers (`unshare`, `open`, `write`, `close`,
//! `mount`, `chdir`, `prctl`, and `capset`) and constructs OS-code-only
//! `io::Error` values on failure. It does not format diagnostics, acquire Rust
//! locks, consult the environment, or drop captured heap values.
//!
//! A direct pre-exec hook also works when the harness is embedded in an
//! arbitrary consumer or libtest executable. Re-executing the current harness
//! binary would require every consumer to dispatch a private launcher marker,
//! and would release temporary mount-source ownership before the launcher had
//! necessarily completed its bind mount.

use std::io as path_std_io;
use std::path::Path;
use std::process::Command;

/// Installs the extension isolation setup as the command's pre-exec hook.
///
/// All path conversion and identity-map formatting happens before `fork`.
/// The hook itself uses only libc operations that are safe between `fork` and
/// `exec` in a multi-threaded harness.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn configure_command(
    command: &mut Command,
    secret_mask_target: Option<&Path>,
    empty_mask: &Path,
    settings_root: Option<&Path>,
    cwd: &Path,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::CommandExt as _;

    fn c_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "launcher path contains a NUL byte".to_owned())
    }

    let secret_mask_target = secret_mask_target.map(c_path).transpose()?;
    let empty_mask = c_path(empty_mask)?;
    let settings_root = settings_root.map(c_path).transpose()?;
    let cwd = c_path(cwd)?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let uid_map = format!("{uid} {uid} 1\n").into_bytes();
    let gid_map = format!("{gid} {gid} 1\n").into_bytes();

    // SAFETY: the closure calls only the allocation-free libc operations in
    // `install_linux_namespace`. All owned input was prepared before `fork`.
    unsafe {
        command.pre_exec(move || {
            install_linux_namespace(
                secret_mask_target.as_deref(),
                &empty_mask,
                settings_root.as_deref(),
                &cwd,
                &uid_map,
                &gid_map,
            )
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn install_linux_namespace(
    secret_mask_target: Option<&std::ffi::CStr>,
    empty_mask: &std::ffi::CStr,
    settings_root: Option<&std::ffi::CStr>,
    cwd: &std::ffi::CStr,
    uid_map: &[u8],
    gid_map: &[u8],
) -> path_std_io::Result<()> {
    fn syscall(result: libc::c_int) -> path_std_io::Result<()> {
        if result == -1 {
            Err(path_std_io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    syscall(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
    match write_proc_file(c"/proc/self/setgroups", b"deny\n") {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(error) => return Err(error),
    }
    write_proc_file(c"/proc/self/uid_map", uid_map)?;
    write_proc_file(c"/proc/self/gid_map", gid_map)?;
    syscall(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    syscall(unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })?;
    if let Some(secret_mask_target) = secret_mask_target {
        syscall(unsafe {
            libc::mount(
                empty_mask.as_ptr(),
                secret_mask_target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        })?;
    }

    if let Some(settings_root) = settings_root {
        syscall(unsafe {
            libc::mount(
                settings_root.as_ptr(),
                settings_root.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        })?;
        syscall(unsafe {
            libc::mount(
                std::ptr::null(),
                settings_root.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND
                    | libc::MS_REMOUNT
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV
                    | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        })?;
    }

    syscall(unsafe { libc::chdir(cwd.as_ptr()) })?;
    drop_linux_capabilities()?;
    syscall(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn write_proc_file(path: &std::ffi::CStr, bytes: &[u8]) -> path_std_io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd == -1 {
        return Err(path_std_io::Error::last_os_error());
    }
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let written = unsafe {
            libc::write(
                fd,
                remaining.as_ptr().cast(),
                remaining.len() as libc::size_t,
            )
        };
        if written == -1 {
            let error = path_std_io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        remaining = &remaining[written as usize..];
    }
    if unsafe { libc::close(fd) } == -1 {
        return Err(path_std_io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn drop_linux_capabilities() -> path_std_io::Result<()> {
    #[repr(C)]
    /// Linux capability ABI request header passed directly to `capset`.
    struct CapabilityHeader {
        /// Linux capability ABI version.
        version: u32,
        /// Zero selects the calling process.
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    /// One 32-bit half of the Linux process capability sets.
    struct CapabilityData {
        /// Capabilities effective for access checks.
        effective: u32,
        /// Capabilities the process may make effective.
        permitted: u32,
        /// Capabilities inherited across exec.
        inheritable: u32,
    }

    for capability in 0..64 {
        let result = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) };
        if result == -1 && path_std_io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
        {
            return Err(path_std_io::Error::last_os_error());
        }
    }
    let ambient_result = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    if ambient_result == -1
        && path_std_io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
    {
        return Err(path_std_io::Error::last_os_error());
    }
    let mut header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            std::ptr::addr_of_mut!(header),
            data.as_mut_ptr(),
        )
    };
    if result == -1 {
        return Err(path_std_io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(not(target_os = "linux"))]
/// Rejects supervised launch isolation on platforms without Linux namespaces.
pub(crate) fn configure_command(
    _command: &mut Command,
    _secret_mask_target: Option<&Path>,
    _empty_mask: &Path,
    _settings_root: Option<&Path>,
    _cwd: &Path,
) -> Result<(), String> {
    Err("supervised extensions require Linux user and mount namespaces".to_owned())
}
