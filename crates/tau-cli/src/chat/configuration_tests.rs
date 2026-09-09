use super::*;

/// Terminal startup must report the original settings error before attempting
/// theme selection, rather than silently starting with default preferences.
#[test]
fn terminal_configuration_preserves_settings_error_identity() {
    let root = tempfile::tempdir().expect("temporary config");
    std::fs::write(root.path().join("cli.yaml"), "show_logo: [invalid\n")
        .expect("invalid settings");
    let dirs = path_tau_config_settings::TauDirs {
        config_dir: Some(root.path().to_path_buf()),
        state_dir: None,
    };
    let expected = tau_config::settings::load_cli_settings_in(&dirs)
        .expect_err("invalid YAML")
        .to_string();
    assert!(
        matches!(
            load_terminal_configuration(&dirs),
            Err(CliError::Participant(message))
                if message == format!("cli.yaml failed to parse:\n{expected}")
        ),
        "retain the contextual error rather than falling back",
    );
}
