use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use tau_config::settings::TauStateAccess;

use super::{IsolationPlan, MountPlan, configure_command};

/// Builds private outer and staging trees for one direct launcher test.
fn mask_roots(temp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let isolation = temp.path().join("isolation");
    fs::create_dir(&isolation).expect("isolation");
    let outer = isolation.join("outer");
    let staging = isolation.join("staging");
    fs::create_dir(&outer).expect("outer");
    fs::create_dir(&staging).expect("staging");
    (outer, staging)
}

/// Proves hidden state omits siblings while restoring only the selected owned,
/// state and settings paths; provider capture storage remains hidden.
#[test]
fn hidden_state_restores_only_approved_nested_mounts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let own = state.join("ext/selected");
    let sibling = state.join("ext/sibling");
    let settings = state.join("providers/selected");
    let capture = state.join("sessions/s1/debug/provider-requests/selected");
    let cwd = temp.path().join("work");
    for directory in [&own, &sibling, &settings, &capture, &cwd] {
        fs::create_dir_all(directory).expect("directory");
    }
    fs::create_dir_all(state.join("secrets")).expect("secrets");
    fs::write(sibling.join("value"), "sibling").expect("sibling sentinel");
    fs::write(settings.join("provider.json"), "settings").expect("settings");
    let state = state.canonicalize().expect("canonical state");
    let own = state.join("ext/selected");
    let settings = state.join("providers/selected");
    let (outer, staging) = mask_roots(&temp);
    let own_source = staging.join("ext/selected");
    let settings_source = staging.join("providers/selected");
    let secret_target = state.join("secrets");
    for target in [&own, &settings] {
        fs::create_dir_all(outer.join(target.strip_prefix(&state).expect("target below state")))
            .expect("outer target");
    }
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test ! -e \"$1/ext/sibling/value\" && test ! -e \"$1/sessions/s1/debug/provider-requests/selected\" && test ! -e \"$2/staging/secrets\" && ! touch \"$2/mutation\" && test \"$(cat \"$1/providers/selected/provider.json\")\" = settings && touch \"$1/ext/selected/owned\" && ! touch \"$1/providers/selected/mutation\"",
        "sh",
        state.to_str().expect("state path"),
        outer
            .parent()
            .expect("isolation parent")
            .to_str()
            .expect("isolation root"),
    ]);
    configure_command(
        &mut command,
        IsolationPlan {
            isolation_root: outer.parent().expect("isolation root"),
            state_root: Some(&state),
            tau_state_access: TauStateAccess::Hidden,
            outer_mask: &outer,
            staging_root: &staging,
            secret_mask_target: Some(&secret_target),
            own_state: Some(MountPlan {
                source: &own_source,
                target: &own,
            }),
            provider_settings: Some(MountPlan {
                source: &settings_source,
                target: &settings,
            }),
            test_nested_mount: None,
            cwd: &cwd,
        },
    )
    .expect("configure isolation");
    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves read-only state keeps ambient data readable but makes every cloned
/// nested mount immutable except the exact extension-owned mount.
/// The provider settings assertion creates a real child bind mount, so it
/// detects a regression from recursive to non-recursive read-only application.
#[test]
fn read_only_state_preserves_only_approved_writable_exceptions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let own = state.join("ext/selected");
    let settings = state.join("providers/selected");
    let nested_settings = settings.join("nested");
    let capture = state.join("sessions/s1/debug/provider-requests/selected");
    let cwd = temp.path().join("work");
    for directory in [
        &own,
        &nested_settings,
        &capture,
        &cwd,
        &state.join("secrets"),
    ] {
        fs::create_dir_all(directory).expect("directory");
    }
    fs::write(state.join("agents"), "ambient").expect("ambient sentinel");
    fs::write(state.join("secrets/value"), "secret").expect("secret sentinel");
    let state = state.canonicalize().expect("canonical state");
    let own = state.join("ext/selected");
    let settings = state.join("providers/selected");
    let secret_target = state.join("secrets");
    let (outer, staging) = mask_roots(&temp);
    let own_source = staging.join("ext/selected");
    let settings_source = staging.join("providers/selected");
    let nested_settings_source = settings_source.join("nested");
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "grep -F \" $1/providers/selected/nested \" /proc/self/mountinfo >/dev/null && test \"$(cat \"$1/agents\")\" = ambient && test ! -e \"$1/secrets/value\" && test ! -e \"$2/staging/secrets\" && ! touch \"$2/mutation\" && ! touch \"$1/ambient-mutation\" && touch \"$1/ext/selected/owned\" && ! touch \"$1/sessions/s1/debug/provider-requests/selected/capture\" && ! touch \"$1/providers/selected/mutation\" && ! touch \"$1/providers/selected/nested/mutation\"",
        "sh",
        state.to_str().expect("state path"),
        outer
            .parent()
            .expect("isolation parent")
            .to_str()
            .expect("isolation root"),
    ]);
    configure_command(
        &mut command,
        IsolationPlan {
            isolation_root: outer.parent().expect("isolation root"),
            state_root: Some(&state),
            tau_state_access: TauStateAccess::ReadOnly,
            outer_mask: &outer,
            staging_root: &staging,
            secret_mask_target: Some(&secret_target),
            own_state: Some(MountPlan {
                source: &own_source,
                target: &own,
            }),
            provider_settings: Some(MountPlan {
                source: &settings_source,
                target: &settings,
            }),
            test_nested_mount: Some(&nested_settings_source),
            cwd: &cwd,
        },
    )
    .expect("configure isolation");
    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves legacy retains ambient state while still masking the mandatory secret
/// root rather than weakening existing secret isolation.
#[test]
fn legacy_state_keeps_ambient_state_but_hides_secrets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let cwd = temp.path().join("work");
    fs::create_dir_all(state.join("secrets")).expect("secrets");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(state.join("visible"), "visible").expect("visible sentinel");
    fs::write(state.join("secrets/value"), "secret").expect("secret sentinel");
    let state = state.canonicalize().expect("canonical state");
    let (outer, staging) = mask_roots(&temp);
    let secret_target = state.join("secrets");
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test \"$(cat \"$1/visible\")\" = visible && test ! -e \"$1/secrets/value\"",
        "sh",
        state.to_str().expect("state path"),
    ]);
    configure_command(
        &mut command,
        IsolationPlan {
            isolation_root: outer.parent().expect("isolation root"),
            state_root: Some(&state),
            tau_state_access: TauStateAccess::Legacy,
            outer_mask: &outer,
            staging_root: &staging,
            secret_mask_target: Some(&secret_target),
            own_state: None,
            provider_settings: None,
            test_nested_mount: None,
            cwd: &cwd,
        },
    )
    .expect("configure isolation");
    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves an absent memory-only host state tree does not require a vacuous
/// mount.
#[test]
fn absent_state_root_starts_in_configured_cwd() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    fs::create_dir_all(&cwd).expect("cwd");
    let (outer, staging) = mask_roots(&temp);
    fs::set_permissions(&outer, fs::Permissions::from_mode(0o700)).expect("mode");
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "test \"$PWD\" = \"$1\"",
        "sh",
        cwd.to_str().expect("cwd"),
    ]);
    configure_command(
        &mut command,
        IsolationPlan {
            isolation_root: outer.parent().expect("isolation root"),
            state_root: None,
            tau_state_access: TauStateAccess::Hidden,
            outer_mask: &outer,
            staging_root: &staging,
            secret_mask_target: None,
            own_state: None,
            provider_settings: None,
            test_nested_mount: None,
            cwd: &cwd,
        },
    )
    .expect("configure isolation");
    assert!(command.status().expect("spawn isolated child").success());
}

/// Proves a failing final cwd setup prevents execution rather than degrading
/// extension isolation.
#[test]
fn setup_failure_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (outer, staging) = mask_roots(&temp);
    let missing = temp.path().join("missing");
    let mut command = Command::new("/bin/true");
    configure_command(
        &mut command,
        IsolationPlan {
            isolation_root: outer.parent().expect("isolation root"),
            state_root: None,
            tau_state_access: TauStateAccess::Hidden,
            outer_mask: &outer,
            staging_root: &staging,
            secret_mask_target: None,
            own_state: None,
            provider_settings: None,
            test_nested_mount: None,
            cwd: &missing,
        },
    )
    .expect("configure isolation");
    assert!(command.status().is_err());
}
