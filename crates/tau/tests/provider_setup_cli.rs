//! Offline CLI coverage for provider setup's persistence boundary.

mod support;

use support::isolated_tau_command;

/// ChatGPT plan sign-in must reject stdout-only setup before prompting or
/// retaining credentials: unlike an API-key profile, its issued registration
/// needs persistent storage even when later model selection is canceled.
#[test]
fn chatgpt_plan_add_rejects_stdout_without_creating_registration() {
    let root = tempfile::tempdir().expect("isolated provider setup root");
    let output = isolated_tau_command(env!("CARGO_BIN_EXE_tau"), root.path())
        .args([
            "provider",
            "add",
            "chatgpt-plan",
            "--config",
            "--output",
            "-",
        ])
        .output()
        .expect("run provider setup without a terminal or credentials");

    assert!(!output.status.success(), "stdout-only sign-in must fail");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 setup error");
    assert!(
        stderr.contains("ChatGPT plan sign-in requires a persistent --state or --config profile")
            && stderr.contains("--output - is unsupported"),
        "reject the unsupported target before attempting interactive sign-in: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "rejected sign-in must not emit a deployable provider profile"
    );
    for path in [
        ".config/tau/providers",
        ".state/tau/providers",
        ".state/tau/secrets",
        ".state/tau/ext",
    ] {
        assert!(
            !root.path().join(path).exists(),
            "rejected sign-in must not create {path}"
        );
    }
}
