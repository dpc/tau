use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use super::configure_command;

/// Proves the process boundary hides every extension instance, while the
/// separately mounted settings snapshot remains readable but immutable.
#[test]
fn process_masks_whole_secret_root_and_mounts_settings_read_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let secrets = temp.path().join("state/secrets");
    let settings = temp.path().join("state/provider-settings/selected");
    let cwd = temp.path().join("work");
    let empty = temp.path().join("empty");
    for directory in [&secrets, &settings, &cwd, &empty] {
        std::fs::create_dir_all(directory).expect("directory");
    }
    std::fs::write(secrets.join("selected"), "selected-secret").expect("selected secret");
    std::fs::write(secrets.join("sibling"), "sibling-secret").expect("sibling secret");
    std::fs::write(settings.join("provider.json"), "settings").expect("settings");

    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test ! -e \"$1/selected\" && test ! -e \"$1/sibling\" && \
         test \"$(cat \"$2/provider.json\")\" = settings && \
         ! touch \"$2/mutation\"",
        "sh",
        secrets.to_str().expect("secret path"),
        settings.to_str().expect("settings path"),
    ]);
    configure_command(&mut command, Some(&secrets), &empty, Some(&settings), &cwd)
        .expect("configure isolation");

    assert!(command.status().expect("spawn isolated child").success());
    assert!(!settings.join("mutation").exists());
}

/// Proves memory-only launch can mask populated Tau state without trying to
/// remount the provider-settings path hidden below that mask.
#[test]
fn memory_only_process_masks_existing_state_without_settings_mount() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let cwd = temp.path().join("work");
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(state.join("secrets/ext/sibling")).expect("state");
    std::fs::create_dir_all(state.join("provider-settings/selected")).expect("settings");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&empty).expect("empty");
    std::fs::write(state.join("secrets/ext/sibling/value"), "secret").expect("secret");

    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test ! -e \"$1/secrets/ext/sibling/value\"",
        "sh",
        state.to_str().expect("state path"),
    ]);
    configure_command(&mut command, Some(&state), &empty, None, &cwd).expect("configure isolation");

    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves the vacuous absent-state mask is unnecessary and does not prevent a
/// memory-only child from starting in its configured working directory.
#[test]
fn memory_only_process_starts_when_state_root_is_absent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&empty).expect("empty");
    fs::set_permissions(&empty, Permissions::from_mode(0o700)).expect("mode");

    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test \"$PWD\" = \"$1\"",
        "sh",
        cwd.to_str().expect("cwd"),
    ]);
    configure_command(&mut command, None, &empty, None, &cwd).expect("configure isolation");

    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves namespace setup failures remain ordinary spawn failures rather than
/// executing the extension without its required isolation.
#[test]
fn process_fails_closed_when_configured_cwd_disappears() {
    let temp = tempfile::tempdir().expect("tempdir");
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&empty).expect("empty");
    let mut command = Command::new("/bin/true");
    configure_command(
        &mut command,
        None,
        &empty,
        None,
        &temp.path().join("missing"),
    )
    .expect("configure isolation");

    assert!(command.status().is_err());
}
