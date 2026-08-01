use std::str::FromStr;
use std::{ffi as path_std_ffi, path as path_std_path};

use tau_config::settings as path_tau_config_settings;
use tau_config::settings::{
    ExtensionCliOverride, ExtensionEntry, HarnessConfigCliOverride, HarnessSettings, ProfileName,
    load_harness_settings_in,
};
use tempfile::TempDir;

use super::*;

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
    let config = resolve_config_in(&dirs).expect("resolve config from supplied dirs");

    assert!(
        !config.extensions.contains_key("core-shell"),
        "headless embedded tests must not accidentally read the developer's global harness config"
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

    let (settings, error) = load_harness_settings_without_environment_or_warn(&dirs);
    assert!(error.is_none());
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
    let profile = ProfileName::parse("focused").expect("profile");
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
    assert_eq!(resolved[1].name, "core-shell");
    assert_eq!(resolved[2].name, "std-notifications");
    assert_eq!(resolved[3].name, "std-websearch");
}

#[test]
fn resolve_extensions_builtin_can_start_disabled() {
    let s = HarnessSettings::built_in();
    let resolved = resolve_extensions(&s, builtins()).expect("resolve");
    assert!(resolved.iter().all(|e| e.name != "test-dummy"));
    assert!(resolved.iter().all(|e| e.name != "std-pim"));
    assert!(resolved.iter().all(|e| e.name != "std-email"));
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
#[test]
fn validate_cli_overrides_rejects_invalid_harness_config_override() {
    let overrides = [
        HarnessConfigCliOverride::from_str("session_retention_days=abc").expect("override syntax"),
    ];

    let err = validate_cli_overrides(&[], &[], &overrides).expect_err("wrong type fails");

    let err = err.to_string();
    assert!(err.contains("invalid type"));
    assert!(err.contains("expected u64"));
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

/// Force a parse of `config/built-in.extensions.yaml` so a
/// malformed file blows up here rather than at user startup.
#[test]
fn built_in_extensions_yaml_parses() {
    let _ = built_in_extension_defs();
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
        "optional extension optional-empty did not initialize"
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
