use std::str::FromStr;
use std::time::Duration;
use std::{ffi as path_std_ffi, path as path_std_path};

use tau_config::settings as path_tau_config_settings;
use tau_config::settings::{
    ExtensionCliOverride, ExtensionEntry, HarnessConfigCliOverride, HarnessSettings,
    ProfileSelection, load_harness_settings_in,
};
use tempfile::TempDir;

use super::*;

/// Ensures arbitrary extension config maps merge recursively while every
/// non-map shape, including nested null, replaces the lower-precedence value.
#[test]
fn extension_config_merge_is_recursive_with_replacement_leaves() {
    let base = serde_json::json!({
        "object": {
            "kept": true,
            "changed": 1,
            "array": [1, 2],
            "to_null": "value",
            "type_change": {"nested": true}
        }
    });
    let over = serde_json::json!({
        "object": {
            "changed": 2,
            "array": [3],
            "to_null": null,
            "type_change": "scalar"
        }
    });

    assert_eq!(
        merge_json(base, over),
        serde_json::json!({
            "object": {
                "kept": true,
                "changed": 2,
                "array": [3],
                "to_null": null,
                "type_change": "scalar"
            }
        })
    );
}

/// Ensures extension resolution applies recursive user config over built-in
/// defaults rather than replacing a complete nested object.
#[test]
fn extension_resolution_recursively_merges_builtin_and_user_config() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.clear();
    settings.extensions.insert(
        "core-shell".to_owned(),
        ExtensionEntry {
            config: Some(serde_json::json!({
                "shell": {
                    "command": "bash",
                    "allowlist": []
                }
            })),
            ..ExtensionEntry::default()
        },
    );
    let resolved = resolve_extensions(
        &settings,
        vec![builtin(
            "core-shell",
            "ext-shell",
            "tool",
            true,
            serde_json::json!({
                "shell": {
                    "command": "sh",
                    "extra_env": {"BASE": "kept"}
                }
            }),
        )],
    )
    .expect("resolve extension");

    assert_eq!(
        resolved[0].config,
        serde_json::json!({
            "shell": {
                "command": "bash",
                "extra_env": {"BASE": "kept"},
                "allowlist": []
            }
        })
    );
}

/// Runtime socket masking resolves fail-closed while preserving one explicit
/// trusted-component legacy opt-out.
#[test]
fn extension_resolution_defaults_runtime_sockets_hidden() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.clear();
    settings.extensions.insert(
        "masked".to_owned(),
        ExtensionEntry {
            command: Some(vec!["masked".to_owned()]),
            ..ExtensionEntry::default()
        },
    );
    settings.extensions.insert(
        "trusted".to_owned(),
        ExtensionEntry {
            command: Some(vec!["trusted".to_owned()]),
            tau_runtime_socket_access: Some(
                path_tau_config_settings::TauRuntimeSocketAccess::Legacy,
            ),
            ..ExtensionEntry::default()
        },
    );

    let resolved = resolve_extensions(&settings, Vec::new()).expect("resolve extensions");
    let masked = resolved
        .iter()
        .find(|extension| extension.name == "masked")
        .expect("masked extension");
    let trusted = resolved
        .iter()
        .find(|extension| extension.name == "trusted")
        .expect("trusted extension");
    assert_eq!(
        masked.tau_runtime_socket_access,
        path_tau_config_settings::TauRuntimeSocketAccess::Hidden
    );
    assert_eq!(
        trusted.tau_runtime_socket_access,
        path_tau_config_settings::TauRuntimeSocketAccess::Legacy
    );
}

fn builtin(
    name: &str,
    suffix_arg: &str,
    role: &str,
    enable: bool,
    config: serde_json::Value,
) -> BuiltinExtension {
    BuiltinExtension {
        name: name.to_owned(),
        prefix: Vec::new(),
        command: vec!["tau".into()],
        suffix: vec!["component".into(), suffix_arg.into()],
        role: Some(role.into()),
        cwd: None,
        enable,
        require: true,
        startup_timeout: Duration::from_secs(DEFAULT_EXTENSION_STARTUP_TIMEOUT_SECONDS),
        config,
        secrets: BTreeMap::new(),
    }
}

fn builtins() -> Vec<BuiltinExtension> {
    vec![
        builtin(
            "provider-builtin",
            "ext-provider-builtin",
            "provider",
            true,
            serde_json::json!({}),
        ),
        builtin(
            "core-shell",
            "ext-shell",
            "tool",
            true,
            serde_json::json!({}),
        ),
        builtin(
            "test-dummy",
            "ext-test-dummy",
            "tool",
            false,
            serde_json::json!({}),
        ),
        builtin(
            "std-notifications",
            "ext-std-notifications",
            "tool",
            true,
            serde_json::json!({ "agent_start": [], "agent_end": [], "agent_idle": [], "agent_idle_all": [] }),
        ),
        BuiltinExtension {
            name: "std-rostra".to_owned(),
            prefix: Vec::new(),
            command: vec!["tau-ext-rostra".into()],
            suffix: Vec::new(),
            role: Some("tool".into()),
            cwd: None,
            enable: false,
            require: true,
            startup_timeout: Duration::from_secs(10),
            config: serde_json::json!({}),
            secrets: BTreeMap::new(),
        },
        builtin(
            "std-websearch",
            "ext-websearch",
            "tool",
            true,
            serde_json::json!({}),
        ),
        builtin("std-pim", "ext-pim", "tool", false, serde_json::json!({})),
        builtin("std-email", "ext-pim", "tool", false, serde_json::json!({})),
    ]
}

#[test]
fn resolve_config_in_uses_supplied_config_dir() {
    let tempdir = TempDir::new().expect("tempdir");
    let config_dir = tempdir.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("harness.yaml"),
        "extensions:\n  core-shell:\n    enable: false\n",
    )
    .expect("write harness config");

    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(tempdir.path().join("state")),
    };
    let config =
        resolve_config_in_without_environment(&dirs).expect("resolve config from supplied dirs");

    assert!(
        !config.extensions.contains_key("core-shell"),
        "headless embedded tests must not accidentally read the developer's global harness config"
    );
}

/// Ensures one accepted settings snapshot remains the authority for extension
/// resolution and runtime baselines even if the source file changes afterward.
#[test]
fn resolved_config_retains_one_coherent_settings_snapshot() {
    let tempdir = TempDir::new().expect("tempdir");
    let config_path = tempdir.path().join("harness.yaml");
    std::fs::write(
        &config_path,
        "session_retention: 37d\nextensions:\n  core-shell:\n    enable: false\n",
    )
    .expect("write valid harness config");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(tempdir.path().to_path_buf()),
        state_dir: None,
    };

    let accepted =
        resolve_config_in_without_environment(&dirs).expect("accept coherent startup snapshot");
    std::fs::write(&config_path, "extensions: [malformed\n")
        .expect("replace source after acceptance");

    assert_eq!(
        accepted.harness_settings.session_retention(),
        Some(std::time::Duration::from_secs(37 * 24 * 60 * 60))
    );
    assert!(!accepted.extensions.contains_key("core-shell"));
    assert!(
        resolve_config_in_without_environment(&dirs).is_err(),
        "a new startup must reject the now-malformed source"
    );
}

/// Ensures deterministic direct-harness startup uses the configured fallback
/// profile through its no-environment settings loader.
#[test]
fn resolve_config_without_environment_uses_configured_default_profile() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::write(
        tempdir.path().join("harness.yaml"),
        r#"
default_profile: focused
profiles:
  focused:
    extensions:
      core-shell:
        enable: false
"#,
    )
    .expect("write harness config");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(tempdir.path().to_path_buf()),
        state_dir: None,
    };

    let settings =
        load_harness_settings_without_environment(&dirs).expect("load configured fallback");
    assert_eq!(settings.extensions["core-shell"].enable, Some(false));

    let config = resolve_config_in_without_environment(&dirs).expect("resolve configured fallback");

    assert!(!config.extensions.contains_key("core-shell"));
}

/// Ensures profile extension toggles name real built-ins or base-configured
/// extensions, so a disabled typo cannot leave the actual built-in enabled.
#[test]
fn profile_extension_targets_must_exist_in_builtins_or_base_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(tempdir.path().to_path_buf()),
        state_dir: None,
    };
    std::fs::write(
        tempdir.path().join("harness.yaml"),
        r#"
extensions:
  custom:
    command: ["custom-extension"]
profiles:
  focused:
    extensions:
      std-pim:
        enable: false
      custom:
        enable: false
"#,
    )
    .expect("write configured profile");
    let profile = ProfileSelection::parse("focused").expect("profile selection");
    validate_profile_extension_targets(&dirs, &profile).expect("known profile targets");

    std::fs::write(
        tempdir.path().join("harness.yaml"),
        r#"
profiles:
  focused:
    extensions:
      std-pmi:
        enable: false
"#,
    )
    .expect("write typo profile");
    let error =
        validate_profile_extension_targets(&dirs, &profile).expect_err("unknown extension target");
    assert!(
        error
            .to_string()
            .contains("configuration profile `focused` changes unknown extension `std-pmi`"),
        "{error}"
    );

    std::fs::write(
        tempdir.path().join("harness.yaml"),
        r#"
profiles:
  focused:
    extensions:
      std-pim:
        enable: false
  later:
    extensions:
      std-pmi:
        enable: false
"#,
    )
    .expect("write ordered profile typo");
    let ordered = ProfileSelection::parse("focused,later").expect("ordered profile selection");
    let error = validate_profile_extension_targets(&dirs, &ordered)
        .expect_err("later profile target must be validated");
    assert!(
        error
            .to_string()
            .contains("configuration profile `later` changes unknown extension `std-pmi`"),
        "{error}"
    );
}

#[test]
fn resolve_extensions_returns_builtins_when_user_config_empty() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 4);
    assert_eq!(resolved[0].name, "provider-builtin");
    assert_eq!(resolved[0].command, "tau");
    assert_eq!(resolved[0].args, vec!["component", "ext-provider-builtin"]);
    assert_eq!(resolved[0].role.as_deref(), Some("provider"));
    assert_eq!(
        resolved[0].component,
        Some(tau_config::settings::BuiltinComponentIdentity::Provider)
    );
    assert_eq!(resolved[1].name, "core-shell");
    assert_eq!(resolved[2].name, "std-notifications");
    assert_eq!(resolved[3].name, "std-websearch");
}

/// Ensures ordinary extensions retain the two-second default while the external
/// Rostra client keeps its migration-aware deadline and executable boundary,
/// and users can replace the deadline for one configured instance.
#[test]
fn extension_startup_timeout_defaults_and_overrides() {
    let builtins = builtin_extensions();
    let rostra = builtins
        .iter()
        .find(|extension| extension.name == "std-rostra")
        .expect("configured Rostra extension");
    assert_eq!(rostra.command, ["tau-ext-rostra"]);
    assert!(rostra.suffix.is_empty());
    assert_eq!(rostra.startup_timeout, Duration::from_secs(10));

    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "std-rostra".to_owned(),
        ExtensionEntry {
            enable: Some(true),
            startup_timeout_seconds: Some(17),
            ..Default::default()
        },
    );
    settings.extensions.insert(
        "external".to_owned(),
        ExtensionEntry {
            command: Some(vec!["external-extension".to_owned()]),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&settings, builtins).expect("resolve extensions");
    assert_eq!(
        resolved
            .iter()
            .find(|extension| extension.name == "std-rostra")
            .expect("enabled Rostra")
            .startup_timeout,
        Duration::from_secs(17)
    );
    assert_eq!(
        resolved
            .iter()
            .find(|extension| extension.name == "external")
            .expect("user extension")
            .startup_timeout,
        Duration::from_secs(DEFAULT_EXTENSION_STARTUP_TIMEOUT_SECONDS)
    );
}

/// Ensures invalid per-extension readiness deadlines fail resolution rather
/// than silently weakening or indefinitely extending startup availability.
#[test]
fn extension_startup_timeout_requires_one_through_3600_seconds() {
    for seconds in [1, 3_600] {
        let mut settings = HarnessSettings::built_in();
        settings.extensions.insert(
            "external".to_owned(),
            ExtensionEntry {
                command: Some(vec!["external-extension".to_owned()]),
                startup_timeout_seconds: Some(seconds),
                ..Default::default()
            },
        );

        assert_eq!(
            resolve_extensions(&settings, builtins())
                .expect("accept inclusive deadline")
                .iter()
                .find(|extension| extension.name == "external")
                .expect("external extension")
                .startup_timeout,
            Duration::from_secs(seconds)
        );
    }
    for seconds in [0, 3_601] {
        let mut settings = HarnessSettings::built_in();
        settings.extensions.insert(
            "external".to_owned(),
            ExtensionEntry {
                command: Some(vec!["external-extension".to_owned()]),
                startup_timeout_seconds: Some(seconds),
                ..Default::default()
            },
        );

        assert_eq!(
            resolve_extensions(&settings, builtins()).expect_err("reject invalid deadline"),
            ResolveExtensionsError::InvalidStartupTimeout {
                name: "external".to_owned(),
                seconds,
            }
        );
    }
}

/// Ensures the readiness deadline follows normal harness-file, drop-in, and
/// command-line override precedence rather than becoming a special config path.
#[test]
fn extension_startup_timeout_layers_through_file_drop_in_and_cli() {
    let temporary = TempDir::new().expect("temporary directory");
    let config_dir = temporary.path();
    std::fs::write(
        config_dir.join("harness.yaml"),
        "extensions:\n  core-shell:\n    startup_timeout_seconds: 1\n",
    )
    .expect("base configuration");
    std::fs::create_dir(config_dir.join("harness.d")).expect("drop-in directory");
    std::fs::write(
        config_dir.join("harness.d/10-startup-timeout.yaml"),
        "extensions:\n  core-shell:\n    startup_timeout_seconds: 3600\n",
    )
    .expect("drop-in configuration");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.to_path_buf()),
        state_dir: None,
    };
    let overrides =
        [
            HarnessConfigCliOverride::from_str("extensions.core-shell.startup_timeout_seconds=17")
                .expect("override syntax"),
        ];

    let settings = load_settings_for_cli_overrides_in(&dirs, None, &[], &overrides)
        .expect("load layered settings");
    let resolved = resolve_extensions(&settings, builtin_extensions()).expect("resolve extensions");
    assert_eq!(
        resolved
            .iter()
            .find(|extension| extension.name == "core-shell")
            .expect("core shell")
            .startup_timeout,
        Duration::from_secs(17)
    );
}

#[test]
fn resolve_extensions_builtin_can_start_disabled() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
    assert!(resolved.iter().all(|e| e.name != "std-pim"));
    assert!(resolved.iter().all(|e| e.name != "std-email"));
}

/// Ensures only ambient recovery emits notices and attributes each notice to
/// the selecting configuration layer.
#[test]
fn tau_state_access_recovery_notices_report_the_selecting_layer() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "core-shell".to_owned(),
        ExtensionEntry {
            tau_state_access: Some(path_tau_config_settings::TauStateAccess::Hidden),
            ..Default::default()
        },
    );

    let resolved =
        resolve_extensions_with_environment_and_cli_overrides(&settings, builtins(), &[], &[])
            .expect("global and instance state access resolve");
    assert_eq!(
        resolved
            .extensions
            .iter()
            .find(|extension| extension.name == "provider-builtin")
            .expect("provider")
            .tau_state_access,
        path_tau_config_settings::TauStateAccess::ReadOnly
    );
    assert_eq!(
        resolved
            .extensions
            .iter()
            .find(|extension| extension.name == "core-shell")
            .expect("core shell")
            .tau_state_access,
        path_tau_config_settings::TauStateAccess::Hidden
    );

    let mut config = config_from_resolved_extensions(resolved.clone(), HarnessSettings::built_in());
    append_tau_state_access_diagnostics(&mut config, &settings, None);
    assert!(config.extension_startup_diagnostics.is_empty());

    settings.tau_state_access = path_tau_config_settings::TauStateAccess::Legacy;
    let resolved =
        resolve_extensions_with_environment_and_cli_overrides(&settings, builtins(), &[], &[])
            .expect("global recovery policy resolves");
    let mut config = config_from_resolved_extensions(resolved.clone(), HarnessSettings::built_in());
    append_tau_state_access_diagnostics(&mut config, &settings, None);
    let provider = config
        .extension_startup_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.extension == "provider-builtin")
        .expect("provider state-access diagnostic");
    assert_eq!(
        provider.kind,
        ExtensionStartupDiagnosticKind::StateAccess {
            source: TauStateAccessSource::GlobalConfiguration
        }
    );
    assert!(provider.message.contains("global harness configuration"));

    let mut forced = resolved;
    apply_tau_state_access_force(
        &mut forced,
        Some(path_tau_config_settings::TauStateAccess::Legacy),
    );
    assert!(
        forced
            .extensions
            .iter()
            .all(|extension| extension.tau_state_access
                == path_tau_config_settings::TauStateAccess::Legacy)
    );
    let mut forced_config = config_from_resolved_extensions(forced, HarnessSettings::built_in());
    append_tau_state_access_diagnostics(
        &mut forced_config,
        &settings,
        Some(path_tau_config_settings::TauStateAccess::Legacy),
    );
    assert!(
        forced_config
            .extension_startup_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind
                == ExtensionStartupDiagnosticKind::StateAccess {
                    source: TauStateAccessSource::EnvironmentForce
                })
    );
    assert!(
        forced_config
            .extension_startup_diagnostics
            .iter()
            .all(|diagnostic| diagnostic
                .message
                .contains("process-wide TAU_EXTENSION_TAU_STATE_ACCESS"))
    );
}

/// Ensures public environment enables are applied after configuration and
/// before ordered CLI disables, including the normally disabled test fixture.
#[test]
fn environment_extension_enables_precede_cli_overrides() {
    let settings = HarnessSettings::built_in();
    let environment = vec!["std-pim".to_owned(), "test-dummy".to_owned()];
    let cli = vec![ExtensionCliOverride::Disable("std-pim".to_owned())];
    let resolved = resolve_extensions_with_environment_and_cli_overrides(
        &settings,
        builtins(),
        &environment,
        &cli,
    )
    .expect("environment and CLI overrides resolve");
    assert!(
        resolved
            .extensions
            .iter()
            .all(|entry| entry.name != "std-pim")
    );
    assert!(
        resolved
            .extensions
            .iter()
            .any(|entry| entry.name == "test-dummy")
    );
}

/// Ensures an environment typo is fatal even when a later CLI operation would
/// otherwise disable every extension, and that the diagnostic names its source.
#[test]
fn unknown_environment_extension_is_source_specific() {
    let error = resolve_extensions_with_environment_and_cli_overrides(
        &HarnessSettings::built_in(),
        builtins(),
        &["missing-extension".to_owned()],
        &[ExtensionCliOverride::DisableAll],
    )
    .expect_err("unknown environment name must fail");
    assert_eq!(
        error,
        ResolveExtensionsError::UnknownEnvironmentOverride("missing-extension".to_owned())
    );
    assert!(error.to_string().contains("TAU_ENABLE_EXTENSIONS"));
}

/// Ensures malformed or non-UTF-8 private transport fails with source context.
#[test]
fn malformed_private_extension_transport_is_fatal() {
    let error = parse_extension_cli_overrides_transport(Some("not-json".into()))
        .expect_err("malformed JSON must fail");
    assert!(error.to_string().contains(EXTENSION_CLI_OVERRIDES_ENV));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let error =
            parse_extension_cli_overrides_transport(Some(path_std_ffi::OsString::from_vec(vec![
                0xff,
            ])))
            .expect_err("non-UTF-8 must fail");
        assert!(error.to_string().contains(EXTENSION_CLI_OVERRIDES_ENV));
    }
}

/// Ensures both settings-bearing private override transports reject malformed
/// JSON and non-Unicode input while identifying the responsible variable.
#[test]
fn malformed_private_settings_override_transports_are_fatal() {
    for variable in [ROLE_CLI_OVERRIDES_ENV, HARNESS_CONFIG_CLI_OVERRIDES_ENV] {
        let error = parse_startup_override_transport::<serde_json::Value>(
            variable,
            Some("not-json".into()),
        )
        .expect_err("malformed JSON must fail");
        assert!(error.to_string().contains(variable));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let error = parse_startup_override_transport::<serde_json::Value>(
                variable,
                Some(path_std_ffi::OsString::from_vec(vec![0xff])),
            )
            .expect_err("non-UTF-8 must fail");
            assert!(error.to_string().contains(variable));
        }
    }
}

#[test]
fn resolve_extensions_enables_disabled_std_pim_builtin() {
    // The standard PIM extension ships disabled. A user opt-in should keep the
    // built-in tau subcommand suffix and place the entry at its built-in order
    // position.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "std-pim".into(),
        ExtensionEntry {
            enable: Some(true),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let pim = resolved
        .iter()
        .find(|e| e.name == "std-pim")
        .expect("std-pim enabled");
    assert_eq!(pim.command, "tau");
    assert_eq!(pim.args, vec!["component", "ext-pim"]);
    assert_eq!(pim.role.as_deref(), Some("tool"));
}

#[test]
fn resolve_extensions_enables_disabled_std_email_builtin() {
    // The legacy standard email extension ships disabled. A user opt-in should
    // keep the built-in tau subcommand suffix and place the entry at its
    // built-in order position.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "std-email".into(),
        ExtensionEntry {
            enable: Some(true),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let email = resolved
        .iter()
        .find(|e| e.name == "std-email")
        .expect("std-email enabled");
    assert_eq!(email.command, "tau");
    assert_eq!(email.args, vec!["component", "ext-pim"]);
    assert_eq!(email.role.as_deref(), Some("tool"));
}

#[test]
fn resolve_extensions_cli_overrides_apply_after_user_config() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );
    s.extensions.insert(
        "test-dummy".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[
            path_tau_config_settings::ExtensionCliOverride::EnableAll,
            path_tau_config_settings::ExtensionCliOverride::Disable("std-websearch".to_owned()),
            path_tau_config_settings::ExtensionCliOverride::Enable("test-dummy".to_owned()),
        ],
    )
    .expect("resolve");
    let names = resolved
        .iter()
        .map(|extension| extension.name.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"core-shell"));
    assert!(names.contains(&"test-dummy"));
    assert!(!names.contains(&"std-websearch"));
}

#[test]
fn resolve_extensions_enable_all_skips_test_dummy_builtin() {
    let s = HarnessSettings::built_in();

    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[path_tau_config_settings::ExtensionCliOverride::EnableAll],
    )
    .expect("resolve");
    let names = resolved
        .iter()
        .map(|extension| extension.name.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"std-pim"));
    assert!(names.contains(&"std-email"));
    assert!(
        !names.contains(&"test-dummy"),
        "the test fixture must require explicit --enable-extension test-dummy"
    );
}

#[test]
fn resolve_extensions_cli_can_enable_disabled_user_extension() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "future-extension".into(),
        ExtensionEntry {
            command: Some(vec!["future-extension".to_owned()]),
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[path_tau_config_settings::ExtensionCliOverride::Enable(
            "future-extension".to_owned(),
        )],
    )
    .expect("resolve");

    assert!(
        resolved
            .iter()
            .any(|extension| extension.name == "future-extension")
    );
}

#[test]
fn resolve_extensions_cli_enable_unknown_extension_errors() {
    // A typo in `--enable-extension` must fail startup instead of being silently
    // ignored, otherwise users cannot tell why their intended extension is missing.
    let s = HarnessSettings::built_in();
    let err = resolve_extensions_with_cli_overrides(
        &s,
        builtins(),
        &[path_tau_config_settings::ExtensionCliOverride::Enable(
            "missing".to_owned(),
        )],
    )
    .expect_err("unknown extension should fail");

    assert_eq!(
        err,
        super::ResolveExtensionsError::UnknownCliOverride("missing".to_owned())
    );
}
/// Ensures malformed command-line configuration values fail against an explicit
/// empty fixture instead of depending on the invoking user's profile.
#[test]
fn harness_config_cli_override_rejects_invalid_value_without_user_configuration() {
    let tempdir = TempDir::new().expect("tempdir");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(tempdir.path().to_path_buf()),
        state_dir: None,
    };
    let overrides =
        [HarnessConfigCliOverride::from_str("session_retention=abc").expect("override syntax")];

    let err = load_settings_for_cli_overrides_in(&dirs, None, &[], &overrides)
        .expect_err("wrong type fails");

    let err = err.to_string();
    assert!(err.contains("retention duration"));
}

#[test]
fn resolve_extensions_disable_drops_entry() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].name, "provider-builtin");
    assert_eq!(resolved[1].name, "std-notifications");
    assert_eq!(resolved[2].name, "std-websearch");
}

#[test]
fn resolve_extensions_prefix_wraps_builtin_command() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            prefix: Some(vec!["ssh".into(), "user@host".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    // argv[0] is the wrapper; original command moves into args.
    assert_eq!(provider.command, "ssh");
    assert_eq!(
        provider.args,
        vec!["user@host", "tau", "component", "ext-provider-builtin"]
    );
    assert_eq!(
        provider.component,
        Some(tau_config::settings::BuiltinComponentIdentity::Provider)
    );
}

#[test]
fn resolve_extensions_user_command_replaces_builtin_command() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            command: Some(vec!["/usr/local/bin/my-provider".into(), "--flag".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    assert_eq!(provider.command, "/usr/local/bin/my-provider");
    assert_eq!(provider.args, vec!["--flag"]);
    // Role is preserved from the built-in default.
    assert_eq!(provider.role.as_deref(), Some("provider"));
    assert_eq!(provider.component, None);
}

#[test]
fn resolve_extensions_adds_user_extension_keys() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "mything".into(),
        ExtensionEntry {
            command: Some(vec!["/usr/local/bin/mything".into()]),
            ..Default::default()
        },
    );
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert_eq!(resolved.len(), 5);
    let mything = resolved
        .iter()
        .find(|e| e.name == "mything")
        .expect("mything");
    assert_eq!(mything.command, "/usr/local/bin/mything");
    assert!(mything.role.is_none());
}

/// Ensures a renamed bundled extension can omit `command` and piggyback on the
/// running Tau executable while retaining its independent tool namespace.
#[test]
fn resolve_extensions_user_suffix_piggybacks_on_current_tau_executable() {
    let mut settings = HarnessSettings::built_in();
    let tool_prefix = tau_proto::ToolNamePrefix::parse("fedi").expect("tool prefix");
    settings.extensions.insert(
        "fedi-slack".into(),
        ExtensionEntry {
            suffix: Some(vec!["component".into(), "ext-slack".into()]),
            role: Some("tool".into()),
            tool_prefix: Some(Some(tool_prefix.clone())),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&settings, builtins()).expect("resolve");
    let extension = resolved
        .iter()
        .find(|extension| extension.name == "fedi-slack")
        .expect("renamed Slack extension");

    assert_eq!(extension.command, current_tau_executable());
    assert_eq!(extension.args, ["component", "ext-slack"]);
    assert_eq!(extension.role.as_deref(), Some("tool"));
    assert_eq!(extension.tool_prefix.as_ref(), Some(&tool_prefix));
}

/// Ensures wrappers cannot silently become the executable when the actual
/// command and Tau-piggyback suffix are both absent.
#[test]
fn resolve_extensions_prefix_only_entry_has_empty_command_error() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "prefix-only".into(),
        ExtensionEntry {
            prefix: Some(vec!["ssh".into(), "host".into()]),
            ..Default::default()
        },
    );

    let error = resolve_extensions(&settings, builtins()).expect_err("must reject prefix only");

    assert_eq!(
        error,
        ResolveExtensionsError::EmptyCommand("prefix-only".to_owned())
    );
    let message = error.to_string();
    assert!(message.contains("extensions.prefix-only.command"));
    assert!(message.contains("extensions.prefix-only.suffix"));
}

/// Ensures an explicitly empty command remains invalid rather than acquiring
/// omitted-command piggyback semantics from a non-empty suffix.
#[test]
fn resolve_extensions_explicit_empty_command_does_not_piggyback() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "explicit-empty".into(),
        ExtensionEntry {
            command: Some(Vec::new()),
            suffix: Some(vec!["component".into(), "ext-test-dummy".into()]),
            ..Default::default()
        },
    );

    let error =
        resolve_extensions(&settings, builtins()).expect_err("explicit empty command must fail");

    assert_eq!(
        error,
        ResolveExtensionsError::EmptyCommand("explicit-empty".to_owned())
    );
}

#[test]
fn resolve_extensions_empty_entry_does_not_re_enable_disabled_builtin() {
    // `extensions: { "test-dummy": {} }` MUST leave the
    // builtin's `enable: false` intact — absent fields mean "no
    // override", not "use the wire default". See review item #4.
    let mut s = HarnessSettings::built_in();
    s.extensions
        .insert("test-dummy".into(), ExtensionEntry::default());
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
    assert!(resolved.iter().all(|e| e.name != "std-pim"));
    assert!(resolved.iter().all(|e| e.name != "std-email"));
}

#[test]
fn resolve_extensions_user_extension_without_command_errors() {
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "broken".into(),
        ExtensionEntry {
            ..Default::default()
        },
    );
    let err = resolve_extensions(&s, builtins()).expect_err("must err");
    assert_eq!(
        err,
        ResolveExtensionsError::EmptyCommand("broken".to_owned())
    );
}

#[test]
fn resolve_extensions_disabled_user_extension_without_command_is_inert() {
    // A disabled custom extension should be a harmless config placeholder. In
    // particular, it must not require a command just to be dropped from the
    // resolved extension set.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "future-extension".into(),
        ExtensionEntry {
            enable: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins()).expect("disabled entry is dropped");

    assert!(resolved.iter().all(|e| e.name != "future-extension"));
}

#[test]
fn resolve_extensions_loads_from_yaml() {
    // End-to-end: a realistic harness.yaml round-trips through the
    // tau-config loader into the tau-harness resolver.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
                extensions: {
                    "core-shell": { enable: false },
                    "test-dummy": { enable: true },
                    "provider-builtin": { prefix: ["ssh", "host"], cwd: "/srv/provider" },
                    mything: { command: ["/bin/foo"] },
                },
            }"#,
    )
    .expect("write");

    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(dir.to_owned()),
        state_dir: None,
    };
    let s = load_harness_settings_in(&dirs).expect("load");
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let names: Vec<&str> = resolved.iter().map(|e| e.name.as_str()).collect();
    // core-shell dropped (disable). test-dummy enabled. provider-builtin
    // kept (prefix-wrapped). mything appended.
    assert_eq!(
        names,
        vec![
            "provider-builtin",
            "test-dummy",
            "std-notifications",
            "std-websearch",
            "mything"
        ]
    );
    let provider = &resolved[0];
    assert_eq!(provider.command, "ssh");
    assert_eq!(
        provider.args,
        vec!["host", "tau", "component", "ext-provider-builtin"]
    );
    assert_eq!(
        provider.cwd.as_deref(),
        Some(std::path::Path::new("/srv/provider"))
    );
}

/// Ensures the real embedded built-in extension config keeps the full
/// std-notifications default shape rather than only the duplicated test
/// fixture.
#[test]
fn built_in_extensions_json5_contains_std_notifications_idle_all_config() {
    let defs = built_in_extension_defs();
    let extension = defs
        .iter()
        .find(|def| def.name == "std-notifications")
        .expect("std-notifications built-in extension");

    assert_eq!(
        extension.config,
        serde_json::json!({
            "agent_start": [],
            "agent_end": [],
            "agent_idle": [],
            "agent_idle_all": [],
        })
    );
}

/// Ensures the real embedded Slack config keeps agent-id prefixes disabled
/// rather than relying only on the extension's deserialization default.
#[test]
fn built_in_extensions_json5_disables_slack_agent_id_prefix() {
    let defs = built_in_extension_defs();
    let extension = defs
        .iter()
        .find(|def| def.name == "std-slack")
        .expect("std-slack built-in extension");

    assert_eq!(
        extension.config,
        serde_json::json!({"prefix_agent_id": false})
    );
}

/// Ensures the bundled Swarm bridge remains inert and optional until an
/// operator provides its pinned endpoint and Configure secret.
#[test]
fn built_in_extensions_json5_contains_disabled_optional_std_swarm() {
    let swarm = built_in_extension_defs()
        .iter()
        .find(|def| def.name == "std-swarm")
        .expect("std-swarm built-in extension");
    assert!(!swarm.enable);
    assert!(!swarm.require);
    assert_eq!(
        swarm.suffix.as_deref(),
        Some(["component".into(), "ext-swarm".into()].as_slice())
    );
}

#[test]
fn built_in_extensions_json5_contains_disabled_std_pim_and_email_alias() {
    // Guard the real embedded JSON5, not the local test fixture, so the
    // disabled-by-default PIM extension and legacy email alias keep the
    // documented tau component suffix and tool role when future built-ins are
    // edited.
    let defs = built_in_extension_defs();
    for name in ["std-pim", "std-email"] {
        let extension = defs
            .iter()
            .find(|def| def.name == name)
            .expect("built-in extension");
        assert!(!extension.enable);
        assert_eq!(
            extension.suffix.as_deref(),
            Some(["component".to_owned(), "ext-pim".to_owned()].as_slice())
        );
        assert_eq!(extension.role.as_deref(), Some("tool"));
    }
}

#[test]
fn resolve_extensions_carries_and_merges_secret_declarations() {
    let mut builtins = builtins();
    builtins[0].secrets.insert(
        "builtin_secret".into(),
        path_tau_config_settings::ExtensionSecretEntry::default(),
    );
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            secrets: Some(BTreeMap::from([(
                "user_secret".into(),
                tau_config::settings::ExtensionSecretEntry { optional: true },
            )])),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    assert!(!provider.secrets["builtin_secret"].optional);
    assert!(provider.secrets["user_secret"].optional);
}

#[test]
fn resolve_extensions_carries_user_extension_cwd() {
    // Extension cwd is harness-owned process launch metadata. It should stay at
    // the extension entry level instead of being mixed into the extension's
    // free-form LifecycleConfigure payload.
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "mything".into(),
        ExtensionEntry {
            command: Some(vec!["/usr/local/bin/mything".into()]),
            cwd: Some(Some(path_std_path::PathBuf::from("/srv/mything"))),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    let mything = resolved
        .iter()
        .find(|e| e.name == "mything")
        .expect("mything");
    assert_eq!(
        mything.cwd.as_deref(),
        Some(std::path::Path::new("/srv/mything"))
    );
}
#[test]
fn resolve_extensions_user_can_clear_builtin_cwd() {
    let mut builtins = builtins();
    builtins[0].cwd = Some(path_std_path::PathBuf::from("/srv/provider"));
    let mut s = HarnessSettings::built_in();
    s.extensions.insert(
        "provider-builtin".into(),
        ExtensionEntry {
            cwd: Some(None),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&s, builtins).expect("resolve");
    let provider = resolved
        .iter()
        .find(|e| e.name == "provider-builtin")
        .expect("provider");
    assert_eq!(provider.cwd, None);
}

#[test]
fn resolve_extensions_drops_disabled_entries_with_secret_declarations() {
    let mut builtins = builtins();
    builtins[2].secrets.insert(
        "required_secret".into(),
        path_tau_config_settings::ExtensionSecretEntry::default(),
    );
    let s = HarnessSettings::built_in();

    let resolved = resolve_extensions(&s, builtins).expect("resolve");

    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
}

#[test]
fn resolve_extensions_require_defaults_true_and_user_can_override_builtin() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "core-shell".into(),
        ExtensionEntry {
            require: Some(false),
            ..Default::default()
        },
    );
    settings.extensions.insert(
        "custom-tool".into(),
        ExtensionEntry {
            command: Some(vec!["custom-tool".into()]),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions(&settings, builtins()).expect("resolve");

    let provider = resolved
        .iter()
        .find(|extension| extension.name == "provider-builtin")
        .expect("provider");
    assert!(provider.require);
    let core_shell = resolved
        .iter()
        .find(|extension| extension.name == "core-shell")
        .expect("core shell");
    assert!(!core_shell.require);
    let custom = resolved
        .iter()
        .find(|extension| extension.name == "custom-tool")
        .expect("custom extension");
    assert!(custom.require);
}

#[test]
fn resolve_extensions_cli_availability_overrides_preserve_require() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "std-pim".into(),
        ExtensionEntry {
            require: Some(false),
            ..Default::default()
        },
    );

    let resolved = resolve_extensions_with_cli_overrides(
        &settings,
        builtins(),
        &[ExtensionCliOverride::Enable("std-pim".to_owned())],
    )
    .expect("resolve");

    let pim = resolved
        .iter()
        .find(|extension| extension.name == "std-pim")
        .expect("pim enabled by cli");
    assert!(!pim.require);
}

#[test]
fn resolve_extensions_optional_empty_command_is_skipped_with_diagnostic() {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.insert(
        "optional-empty".into(),
        ExtensionEntry {
            require: Some(false),
            ..Default::default()
        },
    );

    let resolved =
        resolve_extensions_with_cli_overrides_and_diagnostics(&settings, builtins(), &[])
            .expect("optional empty command should not be fatal");

    assert!(
        resolved
            .extensions
            .iter()
            .all(|extension| extension.name != "optional-empty")
    );
    assert_eq!(resolved.diagnostics.len(), 1);
    assert_eq!(resolved.diagnostics[0].extension, "optional-empty");
    assert_eq!(
        resolved.diagnostics[0].message,
        "optional extension `optional-empty` was skipped: its resolved command is empty. \
         Set `extensions.optional-empty.command` to an executable, configure a Tau subcommand \
         suffix, or disable the extension"
    );
}

#[test]
fn resolve_extensions_required_empty_command_remains_fatal() {
    let mut settings = HarnessSettings::built_in();
    settings
        .extensions
        .insert("required-empty".into(), ExtensionEntry::default());

    let error = resolve_extensions_with_cli_overrides_and_diagnostics(&settings, builtins(), &[])
        .expect_err("required empty command should fail");

    assert!(
        matches!(error, ResolveExtensionsError::EmptyCommand(name) if name == "required-empty")
    );
}

/// Public alias environment layers after the private generic transport for
/// direct harness execution, matching the parent CLI's effective precedence.
#[test]
fn alias_environment_follows_private_generic_daemon_transport() {
    let private = vec![HarnessConfigCliOverride {
        key: "aliases.providers".to_owned(),
        raw_value: r#"{"current":"generic"}"#.to_owned(),
    }];
    let overrides = harness_config_overrides_from_sources(
        Some(
            serde_json::to_string(&private)
                .expect("serialize transport")
                .into(),
        ),
        path_tau_config_settings::ModelReferenceAliasSources {
            provider_environment: Some(r#"{"current":"environment"}"#.into()),
            ..Default::default()
        },
    )
    .expect("parse combined startup inputs");
    assert_eq!(overrides.len(), 2);
    assert_eq!(overrides[0].raw_value, r#"{"current":"generic"}"#);
    assert!(
        overrides[1]
            .raw_value
            .contains(r#""current":"environment""#)
    );

    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "agents:\n  model: current/model\n",
    )
    .expect("write aliased role model");
    let settings =
        path_tau_config_settings::load_harness_settings_with_profile_and_cli_overrides_in(
            &path_tau_config_settings::TauDirs {
                config_dir: Some(td.path().to_path_buf()),
                state_dir: Some(td.path().join("state")),
            },
            None,
            &[],
            &overrides,
        )
        .expect("load receiver-effective settings");
    assert!(settings.roles.values().all(|role| {
        role.model
            .as_ref()
            .is_none_or(|model| model.provider == "environment")
    }));
}
