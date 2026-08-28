//! Fail-closed Linux namespace setup for supervised extensions.
//!
//! The hook prepares every allocation and filesystem path before `fork`.
//! Between `fork` and `exec` it invokes only direct libc system-call wrappers.

use std::io as path_std_io;
use std::path::Path;
use std::process::Command;

use tau_config::settings::{TauRuntimeSocketAccess, TauStateAccess};

/// Prevalidated mount inputs for one supervised extension process.
pub(crate) struct IsolationPlan<'a> {
    /// Parent of every temporary mask and staging path, hidden before exec.
    pub(crate) isolation_root: &'a Path,
    /// Canonical Tau state root when it exists.
    pub(crate) state_root: Option<&'a Path>,
    /// Selected visibility policy.
    pub(crate) tau_state_access: TauStateAccess,
    /// Empty tree that becomes the hidden state-root presentation.
    pub(crate) outer_mask: &'a Path,
    /// Dedicated empty tree that becomes the hidden runtime socket
    /// presentation.
    pub(crate) runtime_socket_mask: &'a Path,
    /// Private bind staging directory outside the Tau state root.
    pub(crate) staging_root: &'a Path,
    /// Mandatory secret target, hidden in every mode.
    pub(crate) secret_mask_target: Option<&'a Path>,
    /// Exact extension-owned state bind restored read-write.
    pub(crate) own_state: Option<MountPlan<'a>>,
    /// Selected provider settings bind restored read-only.
    pub(crate) provider_settings: Option<MountPlan<'a>>,
    /// Canonical directory containing Tau harness runtime sockets.
    pub(crate) runtime_socket_root: &'a Path,
    /// Selected runtime socket visibility policy.
    pub(crate) tau_runtime_socket_access: TauRuntimeSocketAccess,
    /// Test-only staging descendant made into a real child bind mount before
    /// the state presentation is installed.
    #[cfg(test)]
    pub(crate) test_nested_mount: Option<&'a Path>,
    /// Canonical working directory selected before entering the namespace.
    pub(crate) cwd: &'a Path,
}

/// One prevalidated source-to-target bind mount.
#[derive(Clone, Copy)]
pub(crate) struct MountPlan<'a> {
    /// Private staged source path.
    pub(crate) source: &'a Path,
    /// Visible destination below Tau state.
    pub(crate) target: &'a Path,
}

/// One source-to-target bind mount prepared for the post-fork child.
#[cfg(target_os = "linux")]
struct PreExecMount {
    /// NUL-terminated private staged bind source.
    source: std::ffi::CString,
    /// NUL-terminated destination visible to the extension.
    target: std::ffi::CString,
}

/// All allocation-owning inputs consumed by namespace setup after `fork`.
///
/// Construct this only in the parent. The post-fork hook borrows this value and
/// must neither allocate nor inspect Rust process-global state before `exec`.
#[cfg(target_os = "linux")]
struct PreExecIsolationPlan {
    /// Parent of every temporary mask and staging path.
    isolation_root: std::ffi::CString,
    /// Canonical Tau state root when the host has one.
    state_root: Option<std::ffi::CString>,
    /// Effective state-root visibility policy.
    tau_state_access: TauStateAccess,
    /// Empty read-only root used to mask state or secrets.
    outer_mask: std::ffi::CString,
    /// Dedicated empty root used only to mask harness runtime sockets.
    runtime_socket_mask: std::ffi::CString,
    /// Private bind staging root outside Tau state.
    staging_root: std::ffi::CString,
    /// Mandatory secret root mask destination when state exists.
    secret_mask_target: Option<std::ffi::CString>,
    /// Exact extension-owned writable state exception.
    own_state: Option<PreExecMount>,
    /// Exact read-only provider settings exception.
    provider_settings: Option<PreExecMount>,
    /// Harness runtime socket directory hidden from the child by default.
    runtime_socket_root: std::ffi::CString,
    /// Effective runtime socket visibility policy.
    tau_runtime_socket_access: TauRuntimeSocketAccess,
    /// Test-only source descendant made into a real child mount before policy
    /// application, proving recursive mount attributes reach it.
    #[cfg(test)]
    test_nested_mount: Option<std::ffi::CString>,
    /// Canonical extension working directory selected before `fork`.
    cwd: std::ffi::CString,
    /// Caller identity map bytes prepared before `fork`.
    uid_map: Vec<u8>,
    /// Caller group identity map bytes prepared before `fork`.
    gid_map: Vec<u8>,
}

/// Installs the extension isolation setup as the command's pre-exec hook.
///
/// All path conversion and identity-map formatting happens before `fork`. The
/// hook itself uses only allocation-free libc operations. `pre_exec` runs in
/// the forked child while other parent threads may hold allocator or stdio
/// locks, so it delegates exclusively to [`install_linux_namespace`].
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn configure_command(
    command: &mut Command,
    plan: IsolationPlan<'_>,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::CommandExt as _;

    fn c_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "launcher path contains a NUL byte".to_owned())
    }

    let isolation_root = c_path(plan.isolation_root)?;
    let state_root = plan.state_root.map(c_path).transpose()?;
    let outer_mask = c_path(plan.outer_mask)?;
    let runtime_socket_mask = c_path(plan.runtime_socket_mask)?;
    let staging_root = c_path(plan.staging_root)?;
    let secret_mask_target = plan.secret_mask_target.map(c_path).transpose()?;
    let c_mount = |mount: MountPlan<'_>| {
        Ok::<_, String>(PreExecMount {
            source: c_path(mount.source)?,
            target: c_path(mount.target)?,
        })
    };
    let own_state = plan.own_state.map(c_mount).transpose()?;
    let provider_settings = plan.provider_settings.map(c_mount).transpose()?;
    let runtime_socket_root = c_path(plan.runtime_socket_root)?;
    #[cfg(test)]
    let test_nested_mount = plan.test_nested_mount.map(c_path).transpose()?;
    let cwd = c_path(plan.cwd)?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let pre_exec_plan = PreExecIsolationPlan {
        isolation_root,
        state_root,
        tau_state_access: plan.tau_state_access,
        outer_mask,
        runtime_socket_mask,
        staging_root,
        secret_mask_target,
        own_state,
        provider_settings,
        runtime_socket_root,
        tau_runtime_socket_access: plan.tau_runtime_socket_access,
        #[cfg(test)]
        test_nested_mount,
        cwd,
        uid_map: format!("{uid} {uid} 1\n").into_bytes(),
        gid_map: format!("{gid} {gid} 1\n").into_bytes(),
    };

    // SAFETY: the closure does not allocate, lock, or access Rust global state
    // after fork. It only borrows fully prepared C-compatible data and returns
    // direct syscall errors through Command's standard exec-error pipe.
    unsafe {
        command.pre_exec(move || install_linux_namespace(&pre_exec_plan));
    }
    Ok(())
}

/// Installs the prevalidated namespace without allocating after `fork`.
///
/// The caller is the direct `pre_exec` hook. Do not add Rust allocation,
/// formatting, environment access, filesystem traversal, synchronization, or
/// destructors to this path: another thread in the parent may hold those
/// process-global resources across `fork`. Every input has been converted to
/// owned C-compatible storage before fork; this function performs only direct
/// libc/syscall operations and returns any failure to the spawn error pipe.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn install_linux_namespace(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    enter_linux_namespaces(plan)?;
    install_state_mounts(plan)?;
    hide_runtime_socket_when_required(plan)?;
    finish_linux_isolation(plan)
}

/// Creates the private user and mount namespaces before applying mount policy.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn enter_linux_namespaces(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    syscall(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
    match write_proc_file(c"/proc/self/setgroups", b"deny\n") {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(error) => return Err(error),
    }
    write_proc_file(c"/proc/self/uid_map", &plan.uid_map)?;
    write_proc_file(c"/proc/self/gid_map", &plan.gid_map)?;
    syscall(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    syscall(unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })
}

/// Applies the prevalidated extension-state visibility policy inside the new
/// namespace.
#[cfg(target_os = "linux")]
fn install_state_mounts(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    let Some(state_root) = plan.state_root.as_deref() else {
        return Ok(());
    };
    bind(state_root, &plan.staging_root)?;
    make_mount_private(&plan.staging_root)?;
    #[cfg(test)]
    if let Some(test_nested_mount) = plan.test_nested_mount.as_deref() {
        bind(test_nested_mount, test_nested_mount)?;
    }
    install_tau_state_access(plan, state_root)?;
    if let Some(mount) = plan.own_state.as_ref() {
        bind(&mount.source, &mount.target)?;
    }
    if let Some(mount) = plan.provider_settings.as_ref() {
        bind(&mount.source, &mount.target)?;
        make_recursively_read_only(&mount.target)?;
    }
    Ok(())
}

/// Applies the selected visibility policy to Tau-owned state and its secret
/// mask.
#[cfg(target_os = "linux")]
fn install_tau_state_access(
    plan: &PreExecIsolationPlan,
    state_root: &std::ffi::CStr,
) -> path_std_io::Result<()> {
    match plan.tau_state_access {
        TauStateAccess::Hidden => {
            bind(&plan.outer_mask, state_root)?;
            remount_read_only(state_root, false)
        }
        TauStateAccess::ReadOnly => {
            bind(&plan.staging_root, state_root)?;
            make_recursively_read_only(state_root)?;
            install_secret_mask(plan)
        }
        TauStateAccess::Legacy => install_secret_mask(plan),
    }
}

/// Hides the optional secret target without changing the surrounding state
/// policy.
#[cfg(target_os = "linux")]
fn install_secret_mask(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    if let Some(secret_mask_target) = plan.secret_mask_target.as_deref() {
        bind(&plan.outer_mask, secret_mask_target)?;
        remount_read_only(secret_mask_target, false)?;
    }
    Ok(())
}

/// Removes runtime-socket visibility only for the explicitly hidden policy.
#[cfg(target_os = "linux")]
fn hide_runtime_socket_when_required(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    if plan.tau_runtime_socket_access == TauRuntimeSocketAccess::Hidden {
        bind(&plan.runtime_socket_mask, &plan.runtime_socket_root)?;
        remount_read_only(&plan.runtime_socket_root, false)?;
    }
    Ok(())
}

/// Covers staging, enters the prepared directory, and removes child privilege.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn finish_linux_isolation(plan: &PreExecIsolationPlan) -> path_std_io::Result<()> {
    // The staged real state was needed only as a bind source. Existing
    // destination binds retain their mount references after this empty
    // read-only cover hides the entire temporary tree from the child.
    bind(&plan.outer_mask, &plan.isolation_root)?;
    make_recursively_read_only(&plan.isolation_root)?;
    syscall(unsafe { libc::chdir(plan.cwd.as_ptr()) })?;
    drop_linux_capabilities()?;
    syscall(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })
}

/// Converts a direct libc return value into the corresponding operating-system
/// error.
#[cfg(target_os = "linux")]
fn syscall(result: libc::c_int) -> path_std_io::Result<()> {
    if result == -1 {
        Err(path_std_io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Recursively bind-mounts a prevalidated source at its prevalidated
/// destination.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn bind(source: &std::ffi::CStr, target: &std::ffi::CStr) -> path_std_io::Result<()> {
    syscall(unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    })
}

/// Makes a mount private so subsequent child changes cannot propagate outward.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn make_mount_private(target: &std::ffi::CStr) -> path_std_io::Result<()> {
    syscall(unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })
}

/// Rebinds a mount read-only, optionally including its descendants.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn remount_read_only(target: &std::ffi::CStr, recursive: bool) -> path_std_io::Result<()> {
    let recursive_flag = if recursive { libc::MS_REC } else { 0 };
    syscall(unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND
                | libc::MS_REMOUNT
                | recursive_flag
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC,
            std::ptr::null(),
        )
    })
}

/// Applies the recursive Linux mount attributes required for an immutable tree.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn make_recursively_read_only(target: &std::ffi::CStr) -> path_std_io::Result<()> {
    #[repr(C)]
    struct MountAttr {
        /// Linux `mount_attr.attr_set` bitset.
        attr_set: u64,
        /// Linux `mount_attr.attr_clr` bitset.
        attr_clr: u64,
        /// Linux `mount_attr.propagation` bitset.
        propagation: u64,
        /// Linux `mount_attr.userns_fd` selector.
        userns_fd: u64,
    }
    const AT_RECURSIVE: u32 = 0x8000;
    const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
    const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
    const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
    const MOUNT_ATTR_NOEXEC: u64 = 0x0000_0008;
    let mut attributes = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // `mount_setattr(2)` and AT_RECURSIVE require Linux 5.12. Do not fall
    // back to a non-recursive remount on older kernels: inherited nested
    // mounts could stay writable. ENOSYS/EINVAL therefore propagates and
    // fails extension startup closed.
    //
    // SAFETY: `attributes` has the kernel ABI layout and remains live for
    // this synchronous syscall; every scalar argument is prevalidated.
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            target.as_ptr(),
            AT_RECURSIVE,
            std::ptr::addr_of_mut!(attributes),
            std::mem::size_of::<MountAttr>(),
        )
    };
    if result == -1 {
        return Err(path_std_io::Error::last_os_error());
    }
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
    _plan: IsolationPlan<'_>,
) -> Result<(), String> {
    Err("supervised extensions require Linux user and mount namespaces".to_owned())
}
