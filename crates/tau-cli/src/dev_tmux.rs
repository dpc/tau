//! Hidden helper for agent-controlled manual Tau terminal E2E checks.
//!
//! This module starts the current checkout inside a private tmux server with a
//! scratch Tau environment. It is intentionally a manual development helper,
//! not a general daemon launcher and not a sandbox boundary.
//! Its trust and workflow boundary is specified by
//! [`SPEC-tau-cli-dev-tmux`](../specs/SPEC-tau-cli-dev-tmux.md).

use std::{fs as path_std_fs, io as path_std_io};

mod provider_access;

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use provider_access::prepare_provider_access;
use tau_config::settings::TauDirs;

use crate::CliError;
use crate::cli::{
    DevTmuxCommand, DevTmuxCommonArgs, DevTmuxSendArgs, DevTmuxStartArgs, DevTmuxStopArgs,
    DevTmuxTargetArgs,
};

const SCRATCH_MARKER_FILE: &str = ".tau-dev-tmux-scratch";
const SCRATCH_MARKER_CONTENT: &str = "tau dev tmux scratch v1\n";

pub(crate) fn run(command: DevTmuxCommand) -> Result<(), CliError> {
    match command {
        DevTmuxCommand::Start(args) => start(args),
        DevTmuxCommand::Capture(args) => capture(args),
        DevTmuxCommand::Send(args) => send(args),
        DevTmuxCommand::Stop(args) => stop(args),
    }
}

fn start(args: DevTmuxStartArgs) -> Result<(), CliError> {
    let generated_scratch_root = args.common.scratch_root.is_none();
    let env = TmuxEnvironment::new(args.common, args.workdir)?;
    print_generated_scratch_root(&env, generated_scratch_root);
    let provider_access = prepare_provider_access(&TauDirs::default(), &env.tau_state_dir())?;
    prepare_start_environment(&env)?;
    provider_access.copy_allowed_profiles()?;
    provider_access.print_summary();

    let tau_bin = resolve_tau_bin(args.tau_bin)?;
    let command = env.tau_shell_command(&tau_bin, provider_access.provider_extension_enabled())?;
    start_tmux_session(&env.target, args.width, args.height, command)?;
    print_start_summary(&env);

    Ok(())
}

fn prepare_start_environment(env: &TmuxEnvironment) -> Result<(), CliError> {
    prepare_scratch_root(&env.target.scratch_root)?;
    ensure_private_directory(&env.home)?;
    ensure_private_directory(&env.config)?;
    ensure_private_directory(&env.state)?;
    ensure_private_directory(&env.runtime)?;
    prepare_workdir(env)?;
    write_scratch_marker(&env.target.scratch_root)
}

fn prepare_workdir(env: &TmuxEnvironment) -> Result<(), CliError> {
    if env.workdir_is_scratch {
        ensure_private_directory(&env.workdir)
    } else {
        ensure_existing_directory(&env.workdir)
    }
}

fn print_generated_scratch_root(env: &TmuxEnvironment, generated_scratch_root: bool) {
    if generated_scratch_root {
        eprintln!(
            "tau dev tmux: generated scratch root {}",
            env.target.scratch_root.display()
        );
    }
}

fn resolve_tau_bin(tau_bin: Option<PathBuf>) -> Result<PathBuf, CliError> {
    match tau_bin {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Ok(std::env::current_dir()?.join(path)),
        None => Ok(std::env::current_exe()?),
    }
}

fn start_tmux_session(
    target: &TmuxTarget,
    width: u16,
    height: u16,
    command: String,
) -> Result<(), CliError> {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(&target.socket)
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(&target.session)
        .arg("-x")
        .arg(width.to_string())
        .arg("-y")
        .arg(height.to_string())
        .arg(command)
        .output()?;
    ensure_output_success(output, "tmux new-session")?;
    Ok(())
}

fn print_start_summary(env: &TmuxEnvironment) {
    println!("started Tau tmux session `{}`", env.target.session);
    println!("socket: {}", env.target.socket.display());
    println!("scratch root: {}", env.target.scratch_root.display());
    println!("workdir: {}", env.workdir.display());
    println!();
    println!(
        "capture: tau dev tmux capture --scratch-root {} --session {}",
        shell_quote_path(&env.target.scratch_root),
        shell_quote(&env.target.session)
    );
    println!(
        "send:    tau dev tmux send --scratch-root {} --session {} -- <text>",
        shell_quote_path(&env.target.scratch_root),
        shell_quote(&env.target.session)
    );
    println!(
        "stop:    tau dev tmux stop --scratch-root {} --session {}",
        shell_quote_path(&env.target.scratch_root),
        shell_quote(&env.target.session)
    );
}

fn capture(args: DevTmuxTargetArgs) -> Result<(), CliError> {
    let target = TmuxTarget::for_target_command(args.common)?;
    target.validate_helper_owned()?;
    let output = Command::new("tmux")
        .arg("-S")
        .arg(&target.socket)
        .arg("capture-pane")
        .arg("-p")
        .arg("-t")
        .arg(&target.session)
        .arg("-S")
        .arg("-2000")
        .output()?;
    let stdout = ensure_output_success(output, "tmux capture-pane")?;
    print!("{}", String::from_utf8_lossy(&stdout));
    Ok(())
}

fn send(args: DevTmuxSendArgs) -> Result<(), CliError> {
    let target = TmuxTarget::for_target_command(args.target.common)?;
    target.validate_helper_owned()?;
    let text = args.text.join(" ");
    let output = Command::new("tmux")
        .arg("-S")
        .arg(&target.socket)
        .arg("send-keys")
        .arg("-t")
        .arg(&target.session)
        .arg("-l")
        .arg(text)
        .output()?;
    ensure_output_success(output, "tmux send-keys")?;
    if !args.no_enter {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&target.socket)
            .arg("send-keys")
            .arg("-t")
            .arg(&target.session)
            .arg("Enter")
            .output()?;
        ensure_output_success(output, "tmux send-keys Enter")?;
    }
    Ok(())
}

fn stop(args: DevTmuxStopArgs) -> Result<(), CliError> {
    let target = TmuxTarget::for_target_command(args.target.common)?;
    target.validate_helper_owned()?;
    let output = Command::new("tmux")
        .args(stop_tmux_args(&target))
        .output()?;
    ensure_output_success(output, "tmux kill-session")?;
    if args.remove_scratch {
        validate_removable_scratch_root(&target.scratch_root)?;
        std::fs::remove_dir_all(&target.scratch_root)?;
    }
    Ok(())
}

/// Identifies an existing tmux helper target selected by scratch root and
/// session.
struct TmuxTarget {
    /// Helper-owned scratch root that contains the tmux socket and marker file.
    scratch_root: PathBuf,
    /// Name of the tmux session to capture, send input to, or stop.
    session: String,
    /// Path to the private tmux server socket inside the scratch root.
    socket: PathBuf,
}

impl TmuxTarget {
    /// Resolves a target command's tmux root, using the deterministic fallback
    /// for callers that intentionally target the historical default session.
    fn for_target_command(common: DevTmuxCommonArgs) -> Result<Self, CliError> {
        let scratch_root = common
            .scratch_root
            .unwrap_or_else(static_dev_tmux_target_scratch_root);
        Self::from_scratch_root(scratch_root, common.session)
    }

    /// Resolves a start command's tmux root, generating a fresh temporary root
    /// when the user did not request a reusable location.
    fn for_start(common: DevTmuxCommonArgs) -> Result<Self, CliError> {
        let scratch_root = common
            .scratch_root
            .unwrap_or_else(unique_dev_tmux_scratch_root);
        Self::from_scratch_root(scratch_root, common.session)
    }

    fn from_scratch_root(scratch_root: PathBuf, session: String) -> Result<Self, CliError> {
        let scratch_root = absolute_path(scratch_root)?;
        let socket = scratch_root.join("tmux.sock");
        Ok(Self {
            scratch_root,
            session,
            socket,
        })
    }

    fn validate_helper_owned(&self) -> Result<(), CliError> {
        validate_existing_scratch_root(&self.scratch_root)?;
        reject_symlink(&self.socket)
    }
}

/// Fully expanded filesystem layout used to start a scratch Tau in tmux.
struct TmuxEnvironment {
    /// Existing or soon-to-be-created tmux target rooted under scratch state.
    target: TmuxTarget,
    /// Scratch HOME directory passed to the child Tau process.
    home: PathBuf,
    /// Scratch XDG config directory passed to the child Tau process.
    config: PathBuf,
    /// Scratch XDG state directory passed to the child Tau process.
    state: PathBuf,
    /// Scratch XDG runtime directory passed to the child Tau process.
    runtime: PathBuf,
    /// Working directory where the child Tau process starts.
    workdir: PathBuf,
    /// Whether `workdir` is helper-owned scratch state created by this helper.
    workdir_is_scratch: bool,
}

impl TmuxEnvironment {
    fn new(common: DevTmuxCommonArgs, workdir: Option<PathBuf>) -> Result<Self, CliError> {
        let target = TmuxTarget::for_start(common)?;
        let scratch_root = target.scratch_root.clone();
        let (workdir, workdir_is_scratch) = match workdir {
            Some(path) => (absolute_path(path)?, false),
            None => (scratch_root.join("work"), true),
        };
        Ok(Self {
            home: scratch_root.join("home"),
            config: scratch_root.join("config"),
            state: scratch_root.join("state"),
            runtime: scratch_root.join("run"),
            target,
            workdir,
            workdir_is_scratch,
        })
    }

    fn tau_shell_command(
        &self,
        tau_bin: &Path,
        enable_provider_extension: bool,
    ) -> Result<String, CliError> {
        let working_directory_override = serde_json::to_string(&self.workdir.display().to_string())
            .map_err(|error| CliError::Participant(error.to_string()))?;
        let provider_extension_arg = if enable_provider_extension {
            " --enable-extension provider-builtin"
        } else {
            ""
        };
        Ok([
            format!("cd {}", shell_quote_path(&self.workdir)),
            format!(
                "HOME={} XDG_CONFIG_HOME={} XDG_STATE_HOME={} XDG_RUNTIME_DIR={} {} \
                 --disable-extensions-all --enable-extension core-shell{} \
                 --harness-config={}",
                shell_quote_path(&self.home),
                shell_quote_path(&self.config),
                shell_quote_path(&self.state),
                shell_quote_path(&self.runtime),
                shell_quote_path(tau_bin),
                provider_extension_arg,
                shell_quote(&format!(
                    "extensions.core-shell.config.working_directory={working_directory_override}"
                )),
            ),
        ]
        .join(" && "))
    }

    fn tau_state_dir(&self) -> PathBuf {
        self.state.join("tau")
    }
}

fn unique_dev_tmux_scratch_root() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tau-tmux-{}-{nanos:x}-{counter:x}",
        std::process::id()
    ))
}

fn static_dev_tmux_target_scratch_root() -> PathBuf {
    std::env::temp_dir().join("tau-e2e-tmux")
}

fn prepare_scratch_root(path: &Path) -> Result<(), CliError> {
    reject_path_with_parent_components(path)?;
    reject_unsafe_root_shape(path)?;
    if path.exists() {
        reject_symlink(path)?;
        if !path.is_dir() {
            return Err(CliError::Participant(format!(
                "scratch root `{}` exists but is not a directory",
                path.display()
            )));
        }
        if scratch_marker_path(path).exists() {
            validate_existing_scratch_marker(path)?;
        } else {
            return Err(CliError::Participant(format!(
                "scratch root `{}` already exists but was not created by `tau dev tmux`; choose a new path or remove it manually",
                path.display()
            )));
        }
    } else {
        std::fs::create_dir_all(path)?;
    }
    set_private_permissions(path)?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), CliError> {
    reject_symlink(path)?;
    if path.exists() && !path.is_dir() {
        return Err(CliError::Participant(format!(
            "`{}` exists but is not a directory",
            path.display()
        )));
    }
    std::fs::create_dir_all(path)?;
    reject_symlink(path)?;
    set_private_permissions(path)?;
    Ok(())
}

fn ensure_existing_directory(path: &Path) -> Result<(), CliError> {
    reject_symlink(path)?;
    if !path.is_dir() {
        return Err(CliError::Participant(format!(
            "explicit workdir `{}` must already exist and be a directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_removable_scratch_root(path: &Path) -> Result<(), CliError> {
    validate_existing_scratch_root(path)
}

fn validate_existing_scratch_root(path: &Path) -> Result<(), CliError> {
    reject_path_with_parent_components(path)?;
    reject_unsafe_root_shape(path)?;
    reject_symlink(path)?;
    if !path.is_dir() {
        return Err(CliError::Participant(format!(
            "scratch root `{}` does not exist or is not a directory",
            path.display()
        )));
    }
    validate_existing_scratch_marker(path)
}

fn validate_existing_scratch_marker(path: &Path) -> Result<(), CliError> {
    let marker = scratch_marker_path(path);
    let mut file = open_existing_marker_file_for_read(path)?;
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|source| {
        CliError::Participant(format!(
            "refusing to read helper marker `{}` for scratch root `{}`: {source}",
            marker.display(),
            path.display()
        ))
    })?;
    if content != SCRATCH_MARKER_CONTENT {
        return Err(CliError::Participant(format!(
            "refusing to use scratch root `{}` because marker content is not recognized",
            path.display()
        )));
    }
    Ok(())
}

fn open_existing_marker_file_for_read(path: &Path) -> Result<File, CliError> {
    let marker = scratch_marker_path(path);
    validate_marker_file_metadata(path, &marker)?;
    let file = open_marker_file_for_read(&marker).map_err(|source| {
        CliError::Participant(format!(
            "refusing to use scratch root `{}` without helper marker `{}`: {source}",
            path.display(),
            marker.display()
        ))
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CliError::Participant(format!(
            "refusing helper marker `{}` for scratch root `{}` because it is not a regular file",
            marker.display(),
            path.display(),
        )));
    }
    Ok(file)
}

fn open_existing_marker_file_for_write(path: &Path) -> Result<File, CliError> {
    let marker = scratch_marker_path(path);
    validate_marker_file_metadata(path, &marker)?;
    let file = open_marker_file_for_write(&marker, false).map_err(|source| {
        CliError::Participant(format!(
            "refusing to write helper marker `{}` for scratch root `{}`: {source}",
            marker.display(),
            path.display()
        ))
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CliError::Participant(format!(
            "refusing helper marker `{}` for scratch root `{}` because it is not a regular file",
            marker.display(),
            path.display(),
        )));
    }
    Ok(file)
}

fn validate_marker_file_metadata(path: &Path, marker: &Path) -> Result<(), CliError> {
    let metadata = std::fs::symlink_metadata(marker).map_err(|source| {
        CliError::Participant(format!(
            "refusing to use scratch root `{}` without helper marker `{}`: {source}",
            path.display(),
            marker.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(CliError::Participant(format!(
            "refusing helper marker `{}` for scratch root `{}` because it is a symlink",
            marker.display(),
            path.display(),
        )));
    }
    if !metadata.is_file() {
        return Err(CliError::Participant(format!(
            "refusing helper marker `{}` for scratch root `{}` because it is not a regular file",
            marker.display(),
            path.display(),
        )));
    }
    Ok(())
}

fn reject_unsafe_root_shape(path: &Path) -> Result<(), CliError> {
    reject_unsafe_root_shape_with_home(path, std::env::var_os("HOME").map(PathBuf::from))
}

fn reject_unsafe_root_shape_with_home(path: &Path, home: Option<PathBuf>) -> Result<(), CliError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if canonical.parent().is_none() {
        return Err(CliError::Participant(
            "refusing to use filesystem root as tmux scratch root".to_owned(),
        ));
    }
    if let Some(home) = home
        && path_matches_home_root(&canonical, home)?
    {
        return Err(CliError::Participant(
            "refusing to use HOME as tmux scratch root".to_owned(),
        ));
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    if canonical == cwd {
        return Err(CliError::Participant(
            "refusing to use the current working directory as tmux scratch root".to_owned(),
        ));
    }
    Ok(())
}

fn reject_path_with_parent_components(path: &Path) -> Result<(), CliError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CliError::Participant(format!(
            "refusing non-canonical scratch path `{}` containing `..`",
            path.display()
        )));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), CliError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(CliError::Participant(format!(
            "refusing symlink path `{}` in tmux scratch environment",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_scratch_marker(path: &Path) -> Result<(), CliError> {
    let marker = scratch_marker_path(path);
    let mut file = match std::fs::symlink_metadata(&marker) {
        Ok(_) => open_existing_marker_file_for_write(path)?,
        Err(error) if error.kind() == path_std_io::ErrorKind::NotFound => {
            open_marker_file_for_write(&marker, true)?
        }
        Err(error) => return Err(error.into()),
    };
    file.set_len(0)?;
    file.write_all(SCRATCH_MARKER_CONTENT.as_bytes())?;
    Ok(())
}

fn scratch_marker_path(path: &Path) -> PathBuf {
    path.join(SCRATCH_MARKER_FILE)
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn path_matches_home_root(canonical_path: &Path, home: PathBuf) -> Result<bool, CliError> {
    let absolute_home = absolute_path(home)?;
    let canonical_home = absolute_home
        .canonicalize()
        .unwrap_or_else(|_| absolute_home.to_path_buf());
    Ok(canonical_path == canonical_home)
}

#[cfg(unix)]
fn open_marker_file_for_read(path: &Path) -> Result<File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_marker_file_for_read(path: &Path) -> Result<File, std::io::Error> {
    path_std_fs::OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn open_marker_file_for_write(path: &Path, create_new: bool) -> Result<File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = path_std_fs::OpenOptions::new();
    options.write(true).create_new(create_new);
    options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_marker_file_for_write(path: &Path, create_new: bool) -> Result<File, std::io::Error> {
    let mut options = path_std_fs::OpenOptions::new();
    options.write(true).create_new(create_new);
    options.open(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, path_std_fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), CliError> {
    Ok(())
}

fn ensure_output_success(output: Output, command: &str) -> Result<Vec<u8>, CliError> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(CliError::Participant(format!(
        "{command} exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn stop_tmux_args(target: &TmuxTarget) -> Vec<std::ffi::OsString> {
    vec![
        "-S".into(),
        target.socket.as_os_str().to_owned(),
        "kill-session".into(),
        "-t".into(),
        target.session.clone().into(),
    ]
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.display().to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;
