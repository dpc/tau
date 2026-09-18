use super::*;

/// Covers the fixture's opt-in gate so `TAU_VCR=off` or an empty value cannot
/// accidentally run a live provider test outside cassette mode.
#[test]
fn vcr_enabled_requires_active_vcr_mode() {
    assert!(!vcr_enabled(None, false).expect("unset VCR mode is valid"));
    assert!(!vcr_enabled(Some(""), false).expect("empty VCR mode is off"));
    assert!(!vcr_enabled(Some("off"), false).expect("explicit off VCR mode is valid"));
    assert!(vcr_enabled(Some("record-if-missing"), false).is_err());
    assert!(vcr_enabled(Some("record-if-missing"), true).expect("record mode is active"));
    assert!(vcr_enabled(Some("replay-only"), true).expect("replay mode is active"));
    assert!(vcr_enabled(Some("bad-mode"), true).is_err());
}

/// Ensures the VCR fixture writes both bundled extensions as supported
/// component invocations without constructing a live fixture or starting its
/// candidate.
#[test]
fn vcr_fixture_harness_config_uses_component_suffixes() {
    let tempdir = TempDir::new().expect("temporary fixture root");
    let root = tempdir.path().join("vcr-fixture-config");
    let config_dir = root.join("config");
    let work_dir = root.join("work");
    std::fs::create_dir_all(&config_dir).expect("fixture config directory");
    std::fs::create_dir_all(&work_dir).expect("fixture work directory");
    let candidate = root.join("nonexistent-candidate");
    assert!(
        !candidate.exists(),
        "the regression candidate must not be executable"
    );
    let candidate = candidate.display().to_string();
    let fixture = VcrFixture {
        _tempdir: tempdir,
        config_dir: config_dir.clone(),
        state_dir: root.join("state"),
        harness_state_dir: root.join("harness-state"),
        work_dir: work_dir.clone(),
        session_id: "vcr-fixture-config".to_owned(),
    };

    fixture
        .write_harness_config("test/model", &candidate)
        .expect("write fixture config");

    let settings = tau_config::settings::load_harness_settings_in(&tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: None,
    })
    .expect("parse fixture config");
    let expected_command = [candidate.clone()];
    let provider = settings
        .extensions
        .get("provider-builtin")
        .expect("provider fixture extension");
    assert_eq!(
        provider.command.as_deref(),
        Some(expected_command.as_slice())
    );
    let expected_provider_suffix = ["component".to_owned(), "ext-provider-builtin".to_owned()];
    assert_eq!(
        provider.suffix.as_deref(),
        Some(expected_provider_suffix.as_slice())
    );
    let shell = settings
        .extensions
        .get("core-shell")
        .expect("shell fixture extension");
    assert_eq!(shell.command.as_deref(), Some(expected_command.as_slice()));
    let expected_shell_suffix = ["component".to_owned(), "ext-shell".to_owned()];
    assert_eq!(
        shell.suffix.as_deref(),
        Some(expected_shell_suffix.as_slice())
    );
    assert_eq!(
        shell.config.as_ref().and_then(|config| {
            config
                .get("working_directory")
                .and_then(serde_json::Value::as_str)
        }),
        Some(
            work_dir
                .to_str()
                .expect("temporary work directory is Unicode")
        )
    );
}
