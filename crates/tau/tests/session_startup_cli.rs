use std::process::Command;

/// Runs the exact public Tau binary with isolated state roots.
fn tau_command(temp: &tempfile::TempDir) -> Command {
    let mut command = Command::new(std::env::var("CARGO_BIN_EXE_tau").expect("tau binary"));
    command
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"));
    command
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
