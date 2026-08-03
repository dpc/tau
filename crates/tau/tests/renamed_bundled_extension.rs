use std::process::Command;

/// Ensures a renamed bundled extension starts through omitted-command
/// piggybacking and registers its tool under the configured instance prefix.
#[test]
fn renamed_bundled_extension_starts_and_registers_prefixed_tool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let runtime_dir = temp.path().join("runtime");
    let tau_config_dir = config_home.join("tau");
    std::fs::create_dir_all(&tau_config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_home).expect("mkdir state");
    std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
    std::fs::write(
        tau_config_dir.join("harness.yaml"),
        r#"
extensions:
  provider-builtin:
    enable: false
  core-shell:
    enable: false
  std-notifications:
    enable: false
  std-websearch:
    enable: false
  renamed-dummy:
    suffix: [component, ext-test-dummy]
    tool_prefix: repro
"#,
    )
    .expect("write harness config");

    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let output = Command::new(tau_bin)
        .args(["--role", "engineer", "dev", "print-tools"])
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env_remove("TAU_ENABLE_EXTENSIONS")
        .env_remove("TAU_PROFILE")
        .output()
        .expect("run tau print-tools");

    assert!(
        output.status.success(),
        "renamed bundled extension should reach Ready; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tools: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("print-tools JSON output");
    let tools = tools.as_array().expect("tool definition array");
    assert!(
        tools.iter().any(|tool| {
            tool.get("name").and_then(serde_json::Value::as_str) == Some("repro_restart_test_dummy")
        }),
        "prefixed dummy tool should be registered; output:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
