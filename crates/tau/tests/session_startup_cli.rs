mod support;

use std::process::Command;

use support::isolated_tau_command;

/// Runs the exact public Tau binary with isolated state roots.
fn tau_command(temp: &tempfile::TempDir) -> Command {
    isolated_tau_command(
        std::env::var("CARGO_BIN_EXE_tau").expect("tau binary"),
        temp.path(),
    )
}

/// Removed startup flags fail at the public process boundary rather than
/// silently selecting old behavior.
#[test]
fn public_cli_rejects_legacy_attach_and_resume_flags() {
    for flag in ["--attach", "--resume"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = tau_command(&temp)
            .arg(flag)
            .output()
            .expect("run public tau");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
}

/// Root-owned startup options are rejected after a target command by the real
/// Clap process boundary.
#[test]
fn public_cli_rejects_startup_options_after_target_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = tau_command(&temp)
        .args(["resume", "session-1", "--role", "engineer"])
        .output()
        .expect("run public tau");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--role'"));
}

/// Omitted resume targets preserve resume mode through public normalization, so
/// an incompatible ephemeral request fails instead of minting a fresh session.
#[test]
fn public_cli_preserves_omitted_resume_mode_with_ephemeral() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = tau_command(&temp)
        .args(["--ephemeral", "resume"])
        .output()
        .expect("run public tau");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--ephemeral cannot be combined with `tau resume`")
    );
}

/// Ambient launch-only configuration must not prevent attaching to an existing
/// daemon, because attachment reuses the daemon's already accepted settings.
/// This inventory covers every public launch override environment currently
/// consumed by the Tau CLI, so a future attach validation must update this
/// deliberate boundary.
#[test]
fn public_attach_ignores_all_ambient_launch_override_environments() {
    const AMBIENT_LAUNCH_OVERRIDES: &[(&str, &str)] = &[
        ("TAU_PROFILE", ""),
        ("TAU_ENABLE_EXTENSIONS", ","),
        ("TAU_EXTENSION_TAU_STATE_ACCESS", "not-a-mode"),
        ("TAU_PROVIDER_ALIASES", "{"),
        ("TAU_MODEL_ALIASES", "{"),
    ];

    for (variable, value) in AMBIENT_LAUNCH_OVERRIDES {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = tau_command(&temp)
            .env(variable, value)
            .arg("attach")
            .output()
            .expect("run public tau");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !output.status.success(),
            "{variable} must reach attach resolution"
        );
        assert!(
            stderr.contains("no running sessions are available to attach"),
            "{variable} must not alter attach dispatch: {stderr}"
        );
        assert!(
            !stderr.contains("cannot apply"),
            "{variable} must be ignored rather than rejected: {stderr}"
        );
    }
}

/// Explicit launch flags remain errors during attach even though the matching
/// ambient launch-only environments are ignored, preventing a caller from
/// believing an existing daemon was reconfigured.
#[test]
fn public_attach_rejects_explicit_launch_override_flags() {
    for (arguments, variable, value, expected) in [
        (
            &["--profile", "focused", "attach"][..],
            "TAU_PROFILE",
            "",
            "`tau attach` cannot apply --profile",
        ),
        (
            &["--enable-extension", "std-pim", "attach"][..],
            "TAU_ENABLE_EXTENSIONS",
            ",",
            "extension enable/disable overrides",
        ),
        (
            &["--provider-alias", "default=work", "attach"][..],
            "TAU_PROVIDER_ALIASES",
            "{",
            "--provider-alias can only be used",
        ),
        (
            &["--model-alias", "fast=provider/model", "attach"][..],
            "TAU_MODEL_ALIASES",
            "{",
            "--model-alias can only be used",
        ),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = tau_command(&temp)
            .env(variable, value)
            .args(arguments)
            .output()
            .expect("run public tau");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success(), "{arguments:?} must fail");
        assert!(
            stderr.contains(expected),
            "{arguments:?} must retain its explicit rejection over poisoned {variable}: {stderr}"
        );
    }
}

/// Profile environment selection remains active for a command that launches a
/// scratch Tau process, preventing attach's exemption from leaking into other
/// launcher paths.
#[test]
fn public_dev_tmux_still_rejects_ambient_profile_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = tau_command(&temp)
        .env("TAU_PROFILE", "focused")
        .args(["dev", "tmux", "start"])
        .output()
        .expect("run public tau");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("cannot use a configuration profile"),
        "{stderr}"
    );
}
