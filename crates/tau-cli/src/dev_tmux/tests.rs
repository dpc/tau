use super::*;

fn common(scratch_root: &Path) -> DevTmuxCommonArgs {
    DevTmuxCommonArgs {
        scratch_root: scratch_root.to_path_buf(),
        session: "tau-e2e".to_owned(),
    }
}

/// Verifies the generated shell command keeps Tau confined to scratch XDG
/// paths and starts only the local core-shell extension, which is the main
/// safety property of the manual tmux E2E helper.
#[test]
fn tau_shell_command_uses_scratch_environment_and_core_shell_only() {
    let scratch = PathBuf::from("/tmp/tau tmux scratch");
    let workdir = PathBuf::from("/tmp/tau tmux scratch/work #1: ok");
    let env = TmuxEnvironment::new(common(&scratch), Some(workdir)).expect("env builds");

    let command = env
        .tau_shell_command(Path::new("/tmp/tau bin/target/debug/tau"))
        .expect("command builds");

    assert!(command.contains("HOME='/tmp/tau tmux scratch/home'"));
    assert!(command.contains("XDG_CONFIG_HOME='/tmp/tau tmux scratch/config'"));
    assert!(command.contains("XDG_STATE_HOME='/tmp/tau tmux scratch/state'"));
    assert!(command.contains("XDG_RUNTIME_DIR='/tmp/tau tmux scratch/run'"));
    assert!(command.contains("--disable-extensions-all"));
    assert!(command.contains("--enable-extension core-shell"));
    assert!(command.contains(
            "--harness-config='extensions.core-shell.config.working_directory=\"/tmp/tau tmux scratch/work #1: ok\"'"
        ));
}

/// Ensures paths with shell and YAML-sensitive characters are shell-quoted
/// and JSON-quoted before being passed through `--harness-config`, so
/// manual tests can use realistic scratch directories without parser
/// surprises.
#[test]
fn tau_shell_command_quotes_shell_and_harness_config_values() {
    let scratch = PathBuf::from("/tmp/tau'tmux");
    let workdir = PathBuf::from("/tmp/tau'tmux/work #x: y");
    let env = TmuxEnvironment::new(common(&scratch), Some(workdir)).expect("env builds");

    let command = env
        .tau_shell_command(Path::new("/tmp/tau'bin"))
        .expect("command builds");

    assert!(command.contains("HOME='/tmp/tau'\\''tmux/home'"));
    assert!(command.contains("'/tmp/tau'\\''bin'"));
    assert!(command.contains(
            "--harness-config='extensions.core-shell.config.working_directory=\"/tmp/tau'\\''tmux/work #x: y\"'"
        ));
}

/// Protects cleanup from deleting arbitrary directories: even when a user
/// asks for `--remove-scratch`, the helper must see its own marker first.
#[test]
fn removable_scratch_root_requires_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = validate_removable_scratch_root(temp.path()).expect_err("unmarked root refused");

    assert!(error.to_string().contains("without helper marker"));
}

/// Ensures destructive stop cleanup proves helper ownership before connecting
/// to tmux, so an unmarked arbitrary directory cannot direct the helper at an
/// unrelated socket and session.
#[test]
fn stop_remove_scratch_refuses_unmarked_root_before_tmux() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = stop(DevTmuxStopArgs {
        target: DevTmuxTargetArgs {
            common: common(temp.path()),
        },
        remove_scratch: true,
    })
    .expect_err("unmarked root refused before tmux");

    assert!(error.to_string().contains("without helper marker"));
}

/// Requires capture/send/stop targets to validate a recognized helper marker
/// before any tmux command can use the selected socket path.
#[test]
fn target_validation_requires_exact_marker_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join(SCRATCH_MARKER_FILE), "lookalike\n").expect("write marker");
    let target = TmuxTarget::new(common(temp.path())).expect("target");

    let error = target
        .validate_helper_owned()
        .expect_err("bad marker refused");

    assert!(
        error
            .to_string()
            .contains("marker content is not recognized")
    );
}

/// Prevents target commands from following a forged `tmux.sock` symlink after
/// marker validation succeeds, keeping operations tied to the helper-owned
/// scratch tree.
#[cfg(unix)]
#[test]
fn target_validation_rejects_symlink_socket() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join(SCRATCH_MARKER_FILE),
        SCRATCH_MARKER_CONTENT,
    )
    .expect("write marker");
    std::os::unix::fs::symlink("/tmp/other-tmux.sock", temp.path().join("tmux.sock"))
        .expect("symlink");
    let target = TmuxTarget::new(common(temp.path())).expect("target");

    let error = target
        .validate_helper_owned()
        .expect_err("socket symlink refused");

    assert!(error.to_string().contains("refusing symlink path"));
}

/// Prevents an arbitrary existing directory with a lookalike marker filename
/// from being promoted into helper-owned scratch state and later removed by the
/// cleanup path.
#[test]
fn existing_scratch_root_requires_exact_marker_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join(SCRATCH_MARKER_FILE),
        "not a tau dev tmux marker\n",
    )
    .expect("write marker");

    let error = prepare_scratch_root(temp.path()).expect_err("bad marker refused");

    assert!(
        error
            .to_string()
            .contains("marker content is not recognized")
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join(SCRATCH_MARKER_FILE)).expect("marker remains"),
        "not a tau dev tmux marker\n"
    );
}

/// Rejects marker symlinks before reading or reusing an existing scratch root,
/// preventing helper ownership checks from following a forged marker elsewhere.
#[cfg(unix)]
#[test]
fn existing_scratch_root_rejects_symlink_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside-marker");
    std::fs::write(&outside, SCRATCH_MARKER_CONTENT).expect("write outside marker");
    std::os::unix::fs::symlink(&outside, temp.path().join(SCRATCH_MARKER_FILE))
        .expect("marker symlink");

    let error = prepare_scratch_root(temp.path()).expect_err("symlink marker refused");

    assert!(error.to_string().contains("because it is a symlink"));
}

/// Rejects FIFO marker paths using metadata before attempting to read marker
/// content, which prevents target commands from blocking on special files.
#[cfg(unix)]
#[test]
fn existing_scratch_root_rejects_fifo_marker_without_blocking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join(SCRATCH_MARKER_FILE);
    let output = Command::new("mkfifo")
        .arg(&marker)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target = TmuxTarget::new(common(temp.path())).expect("target");
    let error = target
        .validate_helper_owned()
        .expect_err("fifo marker refused");

    assert!(error.to_string().contains("not a regular file"));
}

/// Prevents marker creation from writing through a symlink that appears before
/// marker writing, preserving the invariant that helper markers are local
/// regular files inside the scratch root.
#[cfg(unix)]
#[test]
fn write_scratch_marker_rejects_symlink_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside-marker");
    std::fs::write(&outside, "unchanged\n").expect("write outside marker");
    std::os::unix::fs::symlink(&outside, temp.path().join(SCRATCH_MARKER_FILE))
        .expect("marker symlink");

    let error = write_scratch_marker(temp.path()).expect_err("symlink marker refused");

    assert!(error.to_string().contains("because it is a symlink"));
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside marker remains readable"),
        "unchanged\n"
    );
}

/// Rejects FIFO marker paths using metadata before attempting to write marker
/// content, which prevents start/reuse paths from blocking on special files or
/// mutating anything other than a local regular marker file.
#[cfg(unix)]
#[test]
fn write_scratch_marker_rejects_fifo_marker_without_blocking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join(SCRATCH_MARKER_FILE);
    let output = Command::new("mkfifo")
        .arg(&marker)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let error = write_scratch_marker(temp.path()).expect_err("fifo marker refused");

    assert!(error.to_string().contains("not a regular file"));
}

/// Ensures the existing-marker write path opens and validates the file before
/// truncating it, preserving the post-open regular-file check that closes the
/// metadata/open TOCTOU window.
#[test]
fn existing_marker_write_open_does_not_truncate_before_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join(SCRATCH_MARKER_FILE);
    let original = format!("{SCRATCH_MARKER_CONTENT}extra");
    std::fs::write(&marker, &original).expect("write marker");

    let _file = open_existing_marker_file_for_write(temp.path()).expect("marker opens");

    assert_eq!(
        std::fs::read_to_string(marker).expect("marker remains readable"),
        original
    );
}

/// Allows legitimate helper-owned scratch roots to be reused across start/stop
/// cycles as long as their marker content still proves helper ownership.
#[test]
fn existing_scratch_root_accepts_exact_marker_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join(SCRATCH_MARKER_FILE),
        SCRATCH_MARKER_CONTENT,
    )
    .expect("write marker");

    prepare_scratch_root(temp.path()).expect("valid marked scratch root is reusable");
}

/// Protects the isolated HOME/XDG directories from symlink tricks in a
/// reused scratch tree, preventing accidental writes through to real state.
#[cfg(unix)]
#[test]
fn private_directory_rejects_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let link = temp.path().join("home");
    std::os::unix::fs::symlink(temp.path(), &link).expect("symlink");

    let error = ensure_private_directory(&link).expect_err("symlink refused");

    assert!(error.to_string().contains("refusing symlink path"));
}

/// Protects real repositories and other explicitly requested workdirs from
/// being treated as helper-owned scratch state: validation may inspect them,
/// but it must not chmod or create them.
#[cfg(unix)]
#[test]
fn explicit_existing_workdir_is_not_chmodded() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let workdir = temp.path().join("repo");
    std::fs::create_dir(&workdir).expect("workdir");
    std::fs::set_permissions(&workdir, std::fs::Permissions::from_mode(0o755)).expect("set perms");
    let env = TmuxEnvironment::new(common(&temp.path().join("scratch")), Some(workdir.clone()))
        .expect("env");

    ensure_existing_directory(&env.workdir).expect("external workdir validates");

    let mode = std::fs::metadata(&workdir)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755);
    assert!(!env.workdir_is_scratch);
}

/// Prevents catastrophic cleanup requests for roots that should never be
/// considered helper-owned scratch directories.
#[test]
fn removable_scratch_root_rejects_filesystem_root() {
    let error = validate_removable_scratch_root(Path::new("/")).expect_err("root refused");

    assert!(error.to_string().contains("filesystem root"));
}

/// Ensures HOME-root rejection canonicalizes `$HOME` before comparing, so a
/// symlink spelling of the home directory cannot make the same directory look
/// safe as a scratch root.
#[cfg(unix)]
#[test]
fn unsafe_root_shape_rejects_canonical_home_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let real_home = temp.path().join("real-home");
    let symlink_home = temp.path().join("home-link");
    std::fs::create_dir(&real_home).expect("real home");
    std::os::unix::fs::symlink(&real_home, &symlink_home).expect("home symlink");

    let error = reject_unsafe_root_shape_with_home(&real_home, Some(symlink_home))
        .expect_err("canonical HOME root refused");

    assert!(error.to_string().contains("refusing to use HOME"));
}

/// Ensures `stop` targets only the requested helper session instead of killing
/// every tmux session on the private server socket.
#[test]
fn stop_tmux_args_kill_only_requested_session() {
    let target = TmuxTarget::new(common(Path::new("/tmp/tau-stop-target"))).expect("target");

    let args: Vec<String> = stop_tmux_args(&target)
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        args,
        vec![
            "-S",
            "/tmp/tau-stop-target/tmux.sock",
            "kill-session",
            "-t",
            "tau-e2e"
        ]
    );
}
