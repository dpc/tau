//! Real CLI/component coverage for the declaration-only no-state boundary.

use std::path::Path;
use std::process::{Command, Output};

/// Launch without ambient Tau settings, credentials or daemon discovery inputs.
fn inspect(root: &Path, extension: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tau"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .current_dir(root)
        .args([
            "--disable-extensions-all",
            "--enable-extension",
            extension,
            "dev",
            "preview-declarations",
        ])
        .output()
        .expect("run declaration CLI")
}

/// The representative configured tool branch emits groups/fragments without
/// creating state, runtime discovery metadata, timers or papercut storage.
#[test]
fn declaration_cli_inspects_utils_without_state_or_agent_context() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = root.path().join("config/tau");
    std::fs::create_dir_all(&config).expect("config root");
    std::fs::write(
        config.join("harness.yaml"),
        "extensions:\n  std-utils:\n    tool_prefix: work\n    config:\n      papercut:\n        enable: true\n",
    )
    .expect("utility config");
    std::fs::write(root.path().join("AGENTS.md"), "CONTEXT_DISCOVERY_CANARY")
        .expect("context canary");
    let output = inspect(root.path(), "std-utils");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON inventory");
    assert_eq!(value["runtime_unverified"], true);
    assert_eq!(value["extensions"][0]["outcome"], "complete_declarations");
    let tools = value["extensions"][0]["inventory"]["tools"]
        .as_array()
        .expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["tool"]["name"], "work_timer");
    assert_eq!(tools[1]["tool"]["name"], "work_papercut");
    assert!(tools[1]["prompt_fragment"].is_object());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("CONTEXT_DISCOVERY_CANARY"));
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("runtime").exists());
}

/// Config-owned provider candidates survive inaccessible runtime credentials
/// and invalid state-owned profiles; neither runtime source is consulted or
/// changed.
#[test]
fn declaration_cli_provider_omits_state_profiles_and_credentials() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = root.path().join("config/tau/providers/provider-builtin");
    std::fs::create_dir_all(&config).expect("config profiles");
    std::fs::write(
        config.join("fixture.json"),
        r#"{"kind":"chat_completions","models":[{"id":"candidate"}],"credential":{"kind":"api_key","identity":"0123456789abcdef0123456789abcdef"}}"#,
    )
    .expect("config profile");
    let state = root.path().join("state/tau/providers/provider-builtin");
    std::fs::create_dir_all(&state).expect("state canary root");
    let canary = state.join("fixture.json");
    std::fs::write(&canary, "STATE_PROFILE_MUST_NOT_BE_READ").expect("state canary");
    let output = inspect(root.path(), "provider-builtin");
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON inventory");
    assert_eq!(value["extensions"][0]["outcome"], "partial");
    assert_eq!(value["extensions"][0]["state_settings_omitted"], true);
    assert_eq!(
        value["extensions"][0]["inventory"]["providers"][0]["models"][0]["id"],
        "fixture/candidate",
    );
    assert_eq!(
        std::fs::read_to_string(canary).expect("unchanged state"),
        "STATE_PROFILE_MUST_NOT_BE_READ"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("state/tau"))
            .expect("state")
            .count(),
        1
    );
    assert!(!root.path().join("runtime").exists());
}

/// A real unsupported component must not be started via its ordinary lifecycle.
#[test]
fn declaration_cli_reports_unsupported_without_fallback() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = inspect(root.path(), "test-dummy");
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON inventory");
    assert_eq!(value["extensions"][0]["outcome"], "unsupported");
    assert!(value["extensions"][0]["inventory"].is_null());
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("runtime").exists());
}

/// Both optional and required launch failures retain their configured origin.
#[test]
fn declaration_cli_reports_per_origin_resolution_failures() {
    for (require, timeout) in [(false, 10), (true, 10), (true, 0)] {
        let root = tempfile::tempdir().expect("tempdir");
        let config = root.path().join("config/tau");
        std::fs::create_dir_all(&config).expect("config");
        std::fs::write(
            config.join("harness.yaml"),
            format!("extensions:\n  broken:\n    command: []\n    require: {require}\n    startup_timeout_seconds: {timeout}\n"),
        )
        .expect("config");
        let output = inspect(root.path(), "broken");
        assert!(!output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report");
        assert_eq!(value["resolution_incomplete"], true);
        assert_eq!(value["extensions"].as_array().expect("origins").len(), 1);
        assert_eq!(value["extensions"][0]["instance"], "broken");
        assert_eq!(value["extensions"][0]["outcome"], "unavailable");
        assert!(value["extensions"][0]["inventory"].is_null());
        assert!(!root.path().join("state").exists());
    }
}

/// Unknown selections stop launches while preserving discoverable attribution;
/// malformed config cannot discover origins but still emits an incomplete
/// report.
#[test]
fn declaration_cli_global_resolution_errors_still_emit_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = inspect(root.path(), "unknown-origin");
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(value["resolution_incomplete"], true);
    let origins = value["extensions"].as_array().expect("origins");
    assert!(
        origins
            .iter()
            .any(|entry| entry["instance"] == "unknown-origin")
    );
    assert!(
        origins
            .iter()
            .all(|entry| entry["outcome"] == "unavailable")
    );
    let names: Vec<_> = origins
        .iter()
        .map(|entry| entry["instance"].as_str().expect("name"))
        .collect();
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    let config = root.path().join("config/tau");
    std::fs::create_dir_all(&config).expect("config");
    std::fs::write(
        config.join("harness.yaml"),
        "extensions: [INVALID_CONFIG_CANARY",
    )
    .expect("config");
    let output = inspect(root.path(), "std-utils");
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(value["resolution_incomplete"], true);
    assert_eq!(value["extensions"], serde_json::json!([]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("INVALID_CONFIG_CANARY"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("INVALID_CONFIG_CANARY"));
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("runtime").exists());
}
