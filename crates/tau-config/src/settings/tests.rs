use std::str::FromStr;
use std::{fs as path_std_fs, path as path_std_path, process as path_std_process};

use tempfile::TempDir;

use super::*;
use crate::settings as path_crate_settings;

/// Ensures the emergency state-access override accepts only its exact
/// process-wide recovery tokens rather than silently weakening isolation.
#[test]
fn tau_state_access_environment_is_exact_and_fail_closed() {
    assert_eq!(
        parse_tau_state_access_env(Some("hidden".into())).expect("hidden"),
        Some(TauStateAccess::Hidden)
    );
    assert_eq!(
        parse_tau_state_access_env(Some("read_only".into())).expect("read-only"),
        Some(TauStateAccess::ReadOnly)
    );
    assert_eq!(
        parse_tau_state_access_env(Some("legacy".into())).expect("legacy"),
        Some(TauStateAccess::Legacy)
    );
    assert_eq!(parse_tau_state_access_env(None).expect("absent"), None);
    for invalid in ["", "Hidden", "read-only", "legacy ", "all"] {
        assert!(
            parse_tau_state_access_env(Some(invalid.into())).is_err(),
            "{invalid:?} must fail closed"
        );
    }
}

/// Ensures the supported extension environment grammar trims OWS, preserves
/// first-seen order, and makes duplicate enables idempotent.
#[test]
fn enable_extensions_env_parses_and_deduplicates_names() {
    assert_eq!(
        parse_enable_extensions_env(Some("  std-pim,\tstd-rhai,std-pim ".into()))
            .expect("valid extension names"),
        ["std-pim", "std-rhai"]
    );
    assert!(
        parse_enable_extensions_env(None)
            .expect("absent environment")
            .is_empty()
    );
    assert!(
        parse_enable_extensions_env(Some(" \t".into()))
            .expect("optional whitespace")
            .is_empty()
    );
}

/// Ensures empty elements and non-name characters fail loudly instead of
/// silently changing which extensions run.
#[test]
fn enable_extensions_env_rejects_malformed_items() {
    for value in [
        ",std-pim",
        "std-pim,",
        "std-pim,,std-rhai",
        "std pim",
        "std-pim\n",
    ] {
        let error = parse_enable_extensions_env(Some(value.into()))
            .expect_err("malformed extension environment");
        assert!(
            error.to_string().contains(TAU_ENABLE_EXTENSIONS_ENV),
            "{error}"
        );
    }
}

/// Ensures the grammar is exact and case-preserving rather than shell-like or
/// Unicode-whitespace tolerant.
#[test]
fn enable_extensions_env_rejects_quotes_unicode_and_newlines() {
    for value in ["\"std-pim\"", "std-pim\n", "std\u{a0}pim", "std.pim"] {
        assert!(parse_enable_extensions_env(Some(value.into())).is_err());
    }
    assert_eq!(
        parse_enable_extensions_env(Some("Std-Pim".into())).expect("case-preserving name"),
        ["Std-Pim"]
    );
}

/// Ensures non-UTF-8 environment bytes fail instead of being lossily
/// interpreted.
#[cfg(unix)]
#[test]
fn enable_extensions_env_rejects_non_utf8() {
    use std::os::unix::ffi::OsStringExt;
    assert!(parse_enable_extensions_env(Some(OsString::from_vec(vec![0xff]))).is_err());
}

/// Ensures explicit CLI and environment selectors take precedence over the
/// layered top-level fallback, while no selector leaves no selected profile.
#[test]
fn selected_profile_prefers_cli_over_environment() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "default_profile: configured\n",
    )
    .expect("write default profile");
    let dirs = dirs_with_config(td.path());
    assert_eq!(
        selected_profile_in_from_sources(&dirs, Some("cli"), Some("environment".into()))
            .expect("valid profile selection")
            .expect("CLI selection")
            .as_str(),
        "cli"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, None, Some("environment".into()))
            .expect("valid environment profile")
            .expect("environment selection")
            .as_str(),
        "environment"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, None, None)
            .expect("configured fallback profile")
            .expect("configured selection")
            .as_str(),
        "configured"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, Some("configured"), None)
            .expect("explicit default profile")
            .expect("explicit selection")
            .as_str(),
        "configured"
    );
    assert_eq!(
        selected_profile_from_sources(None, None).expect("absent selection"),
        None
    );
}

/// Ensures empty and non-UTF-8 profile selections fail at the shared API
/// boundary instead of reaching profile lookup as arbitrary strings.
#[test]
fn selected_profile_rejects_invalid_names() {
    assert!(ProfileName::parse("").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        assert!(selected_profile_from_sources(None, Some(OsString::from_vec(vec![0xff]))).is_err());
    }
}

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
    }
}

fn profile_name(value: &str) -> ProfileName {
    ProfileName::parse(value).expect("valid profile name")
}

fn dirs_with_config_and_state(
    config_dir: &std::path::Path,
    state_dir: &std::path::Path,
) -> TauDirs {
    TauDirs {
        config_dir: Some(config_dir.to_path_buf()),
        state_dir: Some(state_dir.to_path_buf()),
    }
}

/// Ensures absent `testing.yaml` is distinguishable from an empty allowlist so
/// `tau dev tmux start` can warn users that provider access was not configured.
#[test]
fn testing_settings_missing_file_returns_none() {
    let td = tempfile::tempdir().expect("tempdir");

    let loaded = load_testing_settings(&dirs_with_config(td.path())).expect("load testing");

    assert_eq!(loaded, None);
}

/// Ensures testing config discovery fails closed for path inspection errors
/// instead of treating them as an absent opt-in file.
#[test]
fn testing_settings_reports_discovery_errors() {
    let td = tempfile::tempdir().expect("tempdir");
    let config_file = td.path().join("not-a-directory");
    std::fs::write(&config_file, "x").expect("write file");
    let dirs = TauDirs {
        config_dir: Some(config_file),
        state_dir: None,
    };

    let error = load_testing_settings(&dirs).expect_err("discovery error reported");

    assert!(error.to_string().contains("failed to inspect"));
}

/// Ensures a non-regular `testing.yaml` path fails closed before the config
/// loader can block on or otherwise interpret it as YAML.
#[test]
fn testing_settings_rejects_non_regular_file() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(td.path().join("testing.yaml")).expect("mkdir testing path");

    let error = load_testing_settings(&dirs_with_config(td.path()))
        .expect_err("non-regular testing config rejected");

    assert!(error.to_string().contains("not a regular file"));
}

/// Ensures a FIFO named `testing.yaml` fails closed using metadata before any
/// read attempt that could block waiting for a writer.
#[cfg(unix)]
#[test]
fn testing_settings_rejects_fifo_without_blocking() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("testing.yaml");
    let output = path_std_process::Command::new("mkfifo")
        .arg(&path)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let error = load_testing_settings(&dirs_with_config(td.path()))
        .expect_err("fifo testing config rejected");

    assert!(error.to_string().contains("not a regular file"));
}

/// Ensures `testing.yaml` parses exact extension/provider targets.
#[test]
fn testing_settings_parses_testing_provider_allowlist() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        td.path().join("testing.yaml"),
        "testing_providers:\n  - extension: provider-builtin\n    provider: chatgpt\n  - extension: provider-work\n    provider: openrouter.work\n",
    )
    .expect("write testing settings");

    let loaded = load_testing_settings(&dirs_with_config(td.path()))
        .expect("load testing")
        .expect("present testing settings");

    assert_eq!(
        loaded.testing_providers,
        vec![
            TestingProvider {
                extension: tau_proto::ExtensionName::parse("provider-builtin").expect("extension"),
                provider: tau_proto::ProviderName::new("chatgpt"),
            },
            TestingProvider {
                extension: tau_proto::ExtensionName::parse("provider-work").expect("extension"),
                provider: tau_proto::ProviderName::new("openrouter.work"),
            },
        ]
    );
}

/// Prevents malformed or path-like extension names in `testing.yaml`.
#[test]
fn testing_settings_rejects_unsafe_provider_names() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        td.path().join("testing.yaml"),
        "testing_providers:\n  - extension: ../provider-builtin\n    provider: chatgpt\n",
    )
    .expect("write testing settings");

    let error = load_testing_settings(&dirs_with_config(td.path()))
        .expect_err("unsafe provider name rejected");

    assert!(error.to_string().contains("extension name"));
}

/// Ensures typos in `testing.yaml` fail closed instead of silently producing an
/// empty allowlist that could be mistaken for a configured provider setup.
#[test]
fn testing_settings_rejects_unknown_fields() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(td.path().join("testing.yaml"), "providers: [chatgpt]\n")
        .expect("write testing settings");

    let error = load_testing_settings(&dirs_with_config(td.path()))
        .expect_err("unknown testing key rejected");

    assert!(error.to_string().contains("unknown field"));
}

/// Ensures `session_retention_days: 0` disables cleanup by returning `None`.
#[test]
fn zero_session_retention_disables_cleanup() {
    let settings = HarnessSettings {
        session_retention_days: 0,
        ..HarnessSettings::built_in()
    };

    assert_eq!(settings.session_retention(), None);
}

/// Ensures non-authoritative diagnostic cleanup defaults to fourteen days and
/// can be disabled independently from whole-session retention.
#[test]
fn diagnostic_retention_has_independent_default_and_disable() {
    let built_in = HarnessSettings::built_in();
    assert_eq!(built_in.diagnostic_retention_days, 14);
    assert_eq!(
        built_in.diagnostic_retention(),
        Some(std::time::Duration::from_secs(14 * 24 * 60 * 60))
    );

    let disabled = HarnessSettings {
        diagnostic_retention_days: 0,
        ..built_in
    };
    assert_eq!(disabled.diagnostic_retention(), None);
}

/// Ensures activating-input waits ship with a five-minute floor and retain the
/// established 1,440-minute ceiling unless users override both bounds.
#[test]
fn wait_timeout_bounds_default_and_override() {
    let built_in = HarnessSettings::built_in();
    assert_eq!(built_in.wait_timeout_bounds(), (5, 1_440));

    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "wait_timeout_minimum_minutes: 7\nwait_timeout_maximum_minutes: 11\n",
    )
    .expect("write harness config");

    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load overridden bounds");
    assert_eq!(settings.wait_timeout_bounds(), (7, 11));
}

/// Ensures an inverted activating-input wait range fails configuration loading
/// instead of creating contradictory silent-clamping behavior.
#[test]
fn wait_timeout_bounds_reject_minimum_above_maximum() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "wait_timeout_minimum_minutes: 12\nwait_timeout_maximum_minutes: 11\n",
    )
    .expect("write invalid harness config");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("inverted wait bounds must fail");
    assert!(
        error
            .to_string()
            .contains("wait_timeout_minimum_minutes must not exceed")
    );
}

/// Ensures activating-input bounds fit the persisted u16-minute wait metadata
/// instead of silently truncating a longer configured deadline.
#[test]
fn wait_timeout_bounds_reject_maximum_above_wait_metadata_range() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        format!(
            "wait_timeout_minimum_minutes: 1\nwait_timeout_maximum_minutes: {}\n",
            u64::from(u16::MAX) + 1
        ),
    )
    .expect("write invalid harness config");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("out-of-range wait maximum must fail");
    assert!(
        error
            .to_string()
            .contains("wait_timeout_maximum_minutes must not exceed")
    );
}

/// Ensures the largest timeout representable by persisted wait metadata remains
/// a valid global maximum.
#[test]
fn wait_timeout_bounds_accept_maximum_wait_metadata_range() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        format!(
            "wait_timeout_minimum_minutes: 1\nwait_timeout_maximum_minutes: {}\n",
            u16::MAX
        ),
    )
    .expect("write maximum harness config");

    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load maximum bounds");
    assert_eq!(settings.wait_timeout_bounds(), (1, u64::from(u16::MAX)));
}

/// Ensures tag policy patterns support exact and terminal-prefix matching while
/// rejecting middle globs that would make policy behavior ambiguous.
#[test]
fn tool_policy_tag_patterns_match_exact_and_prefix_only() {
    let policy: ToolPolicy = serde_yaml_ng::from_str(
        r#"
rules:
  test:
    when:
      model_tags: [shell:*]
    disable_tool_tags: [shell:edit:*]
    enable_tool_tags: [shell:cd]
"#,
    )
    .expect("policy parses");
    let rule = &policy.rules["test"];

    assert!(rule.when.model_tags[0].matches(&tau_proto::ModelTag::new("shell:chatgpt")));
    assert!(rule.disable_tool_tags[0].matches(&tau_proto::ToolTag::new("shell:edit:line")));
    assert!(rule.enable_tool_tags[0].matches(&tau_proto::ToolTag::new("shell:cd")));
    assert!(!rule.enable_tool_tags[0].matches(&tau_proto::ToolTag::new("shell:cd:child")));
    assert!(
        serde_yaml_ng::from_str::<ToolPolicy>(
            r#"rules: {bad: {disable_tool_tags: [shell:*:edit]}}"#
        )
        .is_err()
    );
}

/// Ensures the built-in harness config exposes the ChatGPT shell policy as a
/// normal keyed rule that user configuration can disable by name.
#[test]
fn builtin_tool_policy_rule_is_keyed_and_enabled_by_default() {
    let settings = HarnessSettings::built_in();
    let rule = &settings.tool_policy.rules["builtin.chatgpt-shell"];

    assert!(rule.enable);
    assert_eq!(rule.disable_tool_tags.len(), 1);
    assert_eq!(rule.enable_tool_tags.len(), 5);
}

/// Ensures shell style config accepts the three explicit surfaces and treats a
/// whitespace-only higher-precedence value as a model-default reset.
#[test]
fn tool_policy_shell_style_accepts_values_and_blank_reset() {
    let replace: ToolPolicy =
        serde_yaml_ng::from_str("default_shell_tool_style: replace\n").expect("replace style");
    assert_eq!(
        replace.default_shell_tool_style,
        Some(ShellToolStyle::Replace)
    );
    let reset: ToolPolicy =
        serde_yaml_ng::from_str("default_shell_tool_style: '   '\n").expect("blank reset");
    assert_eq!(reset.default_shell_tool_style, None);
    let invalid = serde_yaml_ng::from_str::<ToolPolicy>("default_shell_tool_style: fuzzy\n");
    assert!(
        invalid.is_err(),
        "unknown shell style must be a config error"
    );
}

/// Ensures a higher-precedence null or blank style clears a lower-layer choice
/// rather than retaining it through generic config layering.
#[test]
fn tool_policy_shell_style_drop_in_resets_lower_value() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "tool_policy:\n  default_shell_tool_style: codex\n",
    )
    .expect("write base");
    std::fs::create_dir_all(td.path().join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        td.path().join("harness.d/10-reset.yaml"),
        "tool_policy:\n  default_shell_tool_style: null\n",
    )
    .expect("write reset");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load reset");

    assert_eq!(settings.tool_policy.default_shell_tool_style, None);
}

/// Ensures the `toolPolicy` and nested `enabled` aliases are normalized before
/// config layers merge with built-in canonical fields.
#[test]
fn user_config_can_disable_builtin_tool_policy_rule_with_enabled_alias() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
toolPolicy:
  rules:
    builtin.chatgpt-shell:
      enabled: false
"#,
    )
    .expect("write harness config");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert!(!settings.tool_policy.rules["builtin.chatgpt-shell"].enable);
}

/// Ensures same-source `enabled`/`enable` conflicts in policy rules are
/// rejected with path context instead of relying on serde duplicate-field
/// errors.
#[test]
fn tool_policy_rule_rejects_enabled_enable_alias_conflict() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
tool_policy:
  rules:
    builtin.chatgpt-shell:
      enabled: false
      enable: true
"#,
    )
    .expect("write harness config");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflicting aliases");

    assert!(
        error.to_string().contains("enabled")
            && error.to_string().contains("enable")
            && error.to_string().contains("builtin.chatgpt-shell"),
        "unexpected error: {error}"
    );
}

/// Ensures higher-precedence user config can disable a built-in keyed policy
/// rule without restating the rule's tag predicates or operations.
#[test]
fn user_config_can_disable_builtin_tool_policy_rule_by_name() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
tool_policy:
  rules:
    builtin.chatgpt-shell:
      enable: false
"#,
    )
    .expect("write harness config");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let rule = &settings.tool_policy.rules["builtin.chatgpt-shell"];

    assert!(!rule.enable);
    assert_eq!(rule.disable_tool_tags.len(), 1);
    assert_eq!(rule.enable_tool_tags.len(), 5);
}

/// Ensures user CLI scalar settings override the built-in defaults.
#[test]
fn cli_settings_user_scalar_override_wins_over_built_in() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
        r#"{ greeting: false, show_thinking: false, osc8_links: false, show_tools: "compact", show_messages: "self-summary", show_status: "minimal" }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_thinking);
    assert!(!s.osc8_links);
    assert_eq!(s.show_tools, ShowTools::Compact);
    assert_eq!(s.show_messages, ShowMessages::SelfSummary);
    assert_eq!(s.show_status, ShowStatus::Minimal);
    assert_eq!(s.theme, CliTheme::Named("tau-plain-dark".to_owned()));
}

/// OSC 8 Markdown links are enabled when no user layer overrides the built-in
/// CLI configuration.
#[test]
fn cli_settings_enable_osc8_links_by_default() {
    assert!(CliSettings::built_in().osc8_links);
}

/// Ensures cli.yaml can select a built-in theme by name.
#[test]
fn cli_settings_theme_override() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), r#"{ theme: "tau-plain-light" }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.theme, CliTheme::Named("tau-plain-light".to_owned()));
}

/// Ensures prompt-draft content stays disabled by default and that a normal
/// cli.d layer can explicitly enable it for one CLI process.
#[test]
fn cli_settings_prompt_draft_content_defaults_false_and_layers() {
    assert!(!CliSettings::built_in().send_prompt_draft_content);

    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::create_dir_all(dir.join("cli.d")).expect("create cli.d");
    std::fs::write(
        dir.join("cli.d").join("10-send-draft-content.yaml"),
        "send_prompt_draft_content: true\n",
    )
    .expect("write drop-in");

    let settings = load_cli_settings_in(&dirs_with_config(dir)).expect("load settings");

    assert!(settings.send_prompt_draft_content);
}

/// Ensures typos in top-level cli.yaml keys fail instead of being ignored.
#[test]
fn cli_settings_reject_unknown_top_level_fields() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), "show_thnking: true\n").expect("write");

    let error = load_cli_settings_in(&dirs_with_config(dir)).expect_err("unknown key should fail");

    assert!(
        error.to_string().contains("show_thnking"),
        "unexpected error: {error}"
    );
}

/// Ensures direct theme parsing rejects empty names while accepting built-in
/// and custom names.
#[test]
fn cli_theme_parse_name_rejects_empty_names() {
    assert_eq!(CliTheme::parse_name("  "), None);
    assert_eq!(
        CliTheme::parse_name("tau-plain-dark"),
        Some(CliTheme::Named("tau-plain-dark".to_owned()))
    );
    assert_eq!(
        CliTheme::parse_name("custom"),
        Some(CliTheme::Named("custom".to_owned()))
    );
}

/// Ensures arbitrary non-empty theme names survive config parsing so the CLI
/// can resolve them to external files under the user's `themes` directory.
#[test]
fn cli_settings_external_theme_name_override() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), r#"{ theme: "solarized" }"#).expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.theme, CliTheme::Named("solarized".to_owned()));
}

/// Ensures user key binding additions preserve built-in chords from
/// lower-precedence config.
#[test]
fn cli_settings_user_binding_keeps_built_in_chords() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
        r#"{ bind: { "C-f": { action: "shell-prompt-edit", command: "pick", trim: true } } }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    // User-overridden key reflects the user's value...
    let cf = s.bind.get("C-f").expect("C-f");
    assert_eq!(cf.action, "shell-prompt-edit");
    assert_eq!(cf.command.as_deref(), Some("pick"));
    // ...and other built-in chords survive the merge.
    let cr = s.bind.get("C-r").expect("C-r");
    assert_eq!(cr.action, "prompt-history-search");
    assert!(cr.trim);
    assert!(
        cr.command
            .as_deref()
            .is_some_and(|command| command.contains("fzf"))
    );
    let built_in = CliSettings::built_in();
    let built_in_cf = built_in.bind.get("C-f").expect("C-f");
    assert!(built_in_cf.command.as_deref().is_some_and(|command| {
        command.contains("--preview") && command.contains("--preview-window 'right,60%,wrap'")
    }));
    assert!(s.bind.contains_key("C-t"));
    assert!(s.bind.contains_key("C-o"));
    assert_eq!(s.bind.get("Enter").expect("Enter").action, "submit-prompt");
    assert_eq!(
        s.bind.get("C-Enter").expect("C-Enter").action,
        "submit-prompt"
    );
    assert_eq!(
        s.bind.get("BackTab").expect("BackTab").action,
        "cycle-role-group"
    );
    assert_eq!(s.bind.get("C-k").expect("C-k").action, "agent-previous");
    assert_eq!(s.bind.get("C-j").expect("C-j").action, "agent-next");
    assert_eq!(s.bind.get("C-b").expect("C-b").action, "agent-pick");
    assert!(!s.bind.contains_key("M-a"));
    assert!(!built_in.bind.contains_key("C-B"));
    assert_eq!(s.bind.get("C-p").expect("C-p").action, "prompt-previous");
    assert_eq!(s.bind.get("C-n").expect("C-n").action, "prompt-next");
}

/// Ensures a user Meta binding survives YAML parsing even though Tau does not
/// ship a built-in Meta chord.
#[test]
fn cli_settings_user_meta_binding_is_configurable() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
        r#"{ bind: { "M-a": { action: "prompt-redo" } } }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.bind.get("M-a").expect("M-a").action, "prompt-redo");
    assert_eq!(s.bind.get("C-b").expect("C-b").action, "agent-pick");
}

/// Ensures user completion additions preserve built-in command prefixes
/// from lower-precedence config.
#[test]
fn cli_settings_user_completion_keeps_built_in_prefixes() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
        r##"{ completions: { "#/": "complete_with_command fzf" } }"##,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.completions.get("#/").map(String::as_str),
        Some("complete_with_command fzf")
    );
    assert_eq!(
        s.completions.get("@").map(String::as_str),
        Some("complete_agents")
    );
    assert_eq!(
        s.completions.get("~").map(String::as_str),
        Some("complete_path")
    );
    assert_eq!(
        s.completions.get("./").map(String::as_str),
        Some("complete_path")
    );
    assert_eq!(
        s.completions.get("/").map(String::as_str),
        Some("complete_path")
    );
}
/// Ensures missing optional cli.yaml files load defaults instead of failing.
#[test]
fn cli_state_load_returns_default_when_file_missing() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    assert_eq!(CliState::load(&dirs), CliState::default());
}

/// Ensures saved CLI settings can be loaded back without losing configured
/// fields.
#[test]
fn cli_state_round_trip_through_save_and_load() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    let original = CliState {
        show_diff: true,
        show_thinking: false,
        show_turn_stats: true,
        redraw_counter: true,
        redraw_history_size: 123,
        show_ui_io: true,
        show_tools: path_crate_settings::ShowTools::SummarizeTurn,
        show_messages: path_crate_settings::ShowMessages::AllSummary,
        notice_level: tau_proto::NoticeLevel::Debug,
        show_status: path_crate_settings::ShowStatus::Minimal,
        show_prompt_scroll_indicator: false,
    };
    original.save(&dirs);
    assert!(td.path().join("cli.json").exists());
    let reloaded = CliState::load(&dirs);
    assert_eq!(reloaded, original);
}

/// Ensures omitted message/tool display settings fall back to the expected
/// visible defaults.
#[test]
fn cli_state_defaults_missing_show_messages_to_all_full() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    std::fs::write(td.path().join("cli.json"), r#"{"show_tools":"compact"}"#).expect("write");

    let loaded = CliState::load(&dirs);
    assert_eq!(loaded.show_messages, crate::settings::ShowMessages::AllFull);
    assert!(loaded.show_prompt_scroll_indicator);
}

/// Ensures legacy `show_tools: on` config remains accepted as the full display
/// mode.
#[test]
fn cli_state_loads_legacy_show_tools_on_as_full() {
    let td = TempDir::new().expect("tempdir");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(td.path().to_path_buf()),
    };
    std::fs::write(td.path().join("cli.json"), r#"{"show_tools":"on"}"#).expect("write");

    let loaded = CliState::load(&dirs);
    assert_eq!(loaded.show_tools, crate::settings::ShowTools::Full);
}

/// Ensures canonical keys from higher-precedence drop-ins are not overwritten
/// by lower-precedence legacy aliases during alias normalization.
#[test]
fn harness_canonical_drop_in_wins_over_legacy_alias() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), "agents:\n  defaultRole: legacy\n")
        .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d").join("10-role.yaml"),
        "agents:\n  default_role: canonical\n",
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(settings.default_role.as_deref(), Some("canonical"));
}

/// Ensures canonical CLI overrides are not overwritten by lower-precedence
/// legacy aliases during alias normalization.
#[test]
fn harness_canonical_cli_override_wins_over_legacy_alias() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), "agents:\n  defaultRole: legacy\n")
        .expect("write base");
    let override_ =
        HarnessConfigCliOverride::from_str("agents.default_role=cli").expect("override");

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &[override_])
            .expect("load");

    assert_eq!(settings.default_role.as_deref(), Some("cli"));
}

/// Ensures a single source cannot specify both nested agent legacy and
/// canonical keys.
#[test]
fn harness_rejects_same_layer_agents_alias_conflict() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        "agents:\n  defaultRole: legacy\n  default_role: canonical\n",
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflicting aliases");

    assert!(
        error.to_string().contains("defaultRole") && error.to_string().contains("default_role"),
        "unexpected error: {error}"
    );
}

/// Ensures nested alias/canonical conflicts in one source produce explicit
/// config errors.
#[test]
fn harness_rejects_same_layer_nested_alias_conflict() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          idTemplate: legacy-{{random_alphanumeric 4}}
          id_template: canonical-{{random_alphanumeric 4}}
        "#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflicting aliases");

    assert!(
        error.to_string().contains("idTemplate") && error.to_string().contains("id_template"),
        "unexpected error: {error}"
    );
}

/// Ensures every maintained file-layer legacy alias normalizes to its canonical
/// key.
#[test]
fn harness_file_alias_table_normalizes_all_legacy_keys() {
    let mut value = serde_json::json!({
        "customPrompts": [],
        "toolPolicy": {
            "rules": {
                "builtin.chatgpt-shell": {
                    "enabled": false,
                }
            }
        },
        "agents": {
            "defaultRole": "manager",
            "idTemplate": "agent-{{random_alphanumeric 4}}",
            "displayNameTemplate": "Agent {{n}}",
            "promptFragments": [],
            "requiredSkills": [],
            "contextSizeAlerts": {},
            "roleGroups": {
                "engineer": {
                    "enabled": true,
                    "thinkingSummary": "auto",
                    "serviceTier": "default",
                    "promptFragments": [],
                    "promptOverride": "built-in",
                    "disableToolTags": [],
                    "enableToolTags": [],
                    "disableToolGroups": [],
                    "enableToolGroups": [],
                    "disableTools": [],
                    "enableTools": [],
                    "requiredSkills": [],
                    "contextSizeAlerts": {},
                    "roles": {
                        "engineer": {
                            "enabled": true,
                            "thinkingSummary": "auto",
                            "serviceTier": "default",
                            "promptFragments": [],
                            "promptOverride": "built-in",
                            "disableToolTags": [],
                            "enableToolTags": [],
                            "disableToolGroups": [],
                            "enableToolGroups": [],
                            "disableTools": [],
                            "enableTools": [],
                            "requiredSkills": [],
                            "contextSizeAlerts": {},
                        }
                    }
                }
            }
        }
    });
    let root = value.as_object_mut().expect("root map");
    root.insert(
        "waitTimeoutMinimumMinutes".to_owned(),
        serde_json::Value::from(5),
    );
    root.insert(
        "waitTimeoutMaximumMinutes".to_owned(),
        serde_json::Value::from(1_440),
    );
    for pointer in [
        "/agents/roleGroups/engineer",
        "/agents/roleGroups/engineer/roles/engineer",
    ] {
        let map = value
            .pointer_mut(pointer)
            .and_then(serde_json::Value::as_object_mut)
            .expect("role map");
        map.insert(
            "interSessionReceiver".to_owned(),
            serde_json::Value::Bool(true),
        );
        map.insert(
            "interSessionAutoStart".to_owned(),
            serde_json::Value::Bool(true),
        );
    }
    let agents = value
        .pointer_mut("/agents")
        .and_then(serde_json::Value::as_object_mut)
        .expect("agents map");
    agents.insert("enabled".to_owned(), serde_json::Value::Bool(true));
    agents.insert(
        "thinkingSummary".to_owned(),
        serde_json::Value::String("auto".to_owned()),
    );
    agents.insert(
        "serviceTier".to_owned(),
        serde_json::Value::String("default".to_owned()),
    );

    normalize_harness_config_value(&mut value, "test").expect("normalize");

    assert!(value.get("custom_prompts").is_some());
    assert_eq!(value["wait_timeout_minimum_minutes"], serde_json::json!(5));
    assert_eq!(
        value["wait_timeout_maximum_minutes"],
        serde_json::json!(1440)
    );
    assert!(value.get("tool_policy").is_some());
    assert!(
        value
            .pointer("/tool_policy/rules/builtin.chatgpt-shell/enable")
            .is_some()
    );
    assert!(value.pointer("/agents/enable").is_some());
    assert!(value.pointer("/agents/default_role").is_some());
    assert!(value.pointer("/agents/id_template").is_some());
    assert!(value.pointer("/agents/display_name_template").is_some());
    assert!(value.pointer("/agents/prompt_fragments").is_some());
    assert!(value.pointer("/agents/required_skills").is_some());
    assert!(value.pointer("/agents/context_size_alerts").is_some());
    assert!(value.pointer("/agents/thinking_summary").is_some());
    assert!(value.pointer("/agents/service_tier").is_some());
    let group = value
        .pointer("/agents/role_groups/engineer")
        .expect("group");
    for key in [
        "enable",
        "inter_session_receiver",
        "inter_session_auto_start",
        "thinking_summary",
        "service_tier",
        "prompt_fragments",
        "prompt_override",
        "disable_tool_tags",
        "enable_tool_tags",
        "disable_tool_groups",
        "enable_tool_groups",
        "disable_tools",
        "enable_tools",
        "required_skills",
        "context_size_alerts",
    ] {
        assert!(group.get(key).is_some(), "missing group key {key}");
        assert!(
            group.pointer(&format!("/roles/engineer/{key}")).is_some(),
            "missing role key {key}"
        );
    }
}

/// Ensures every maintained CLI override legacy alias normalizes to its
/// canonical path.
#[test]
fn harness_cli_alias_table_normalizes_all_legacy_keys() {
    let cases = [
        ("customPrompts", "custom_prompts"),
        ("toolPolicy", "tool_policy"),
        ("waitTimeoutMinimumMinutes", "wait_timeout_minimum_minutes"),
        ("waitTimeoutMaximumMinutes", "wait_timeout_maximum_minutes"),
        ("agents.enabled", "agents.enable"),
        ("extensions.work.toolPrefix", "extensions.work.tool_prefix"),
        ("agents.defaultRole", "agents.default_role"),
        ("agents.promptFragments", "agents.prompt_fragments"),
        ("agents.requiredSkills", "agents.required_skills"),
        ("agents.thinkingSummary", "agents.thinking_summary"),
        ("agents.serviceTier", "agents.service_tier"),
        (
            "agents.contextSizeAlerts.compact-soon.enable",
            "agents.context_size_alerts.compact-soon.enable",
        ),
        ("agents.idTemplate", "agents.id_template"),
        ("agents.displayNameTemplate", "agents.display_name_template"),
        (
            "toolPolicy.rules.local.enabled",
            "tool_policy.rules.local.enable",
        ),
        (
            "agents.roleGroups.engineer.enabled",
            "agents.role_groups.engineer.enable",
        ),
        (
            "agents.roleGroups.engineer.contextSizeAlerts.compact-soon.enable",
            "agents.role_groups.engineer.context_size_alerts.compact-soon.enable",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.contextSizeAlerts.compact-soon.enable",
            "agents.role_groups.engineer.roles.engineer.context_size_alerts.compact-soon.enable",
        ),
        (
            "agents.roleGroups.engineer.interSessionReceiver",
            "agents.role_groups.engineer.inter_session_receiver",
        ),
        (
            "agents.roleGroups.engineer.interSessionAutoStart",
            "agents.role_groups.engineer.inter_session_auto_start",
        ),
        (
            "agents.roleGroups.engineer.thinkingSummary",
            "agents.role_groups.engineer.thinking_summary",
        ),
        (
            "agents.roleGroups.engineer.serviceTier",
            "agents.role_groups.engineer.service_tier",
        ),
        (
            "agents.roleGroups.engineer.promptFragments",
            "agents.role_groups.engineer.prompt_fragments",
        ),
        (
            "agents.roleGroups.engineer.promptOverride",
            "agents.role_groups.engineer.prompt_override",
        ),
        (
            "agents.roleGroups.engineer.disableToolTags",
            "agents.role_groups.engineer.disable_tool_tags",
        ),
        (
            "agents.roleGroups.engineer.enableToolTags",
            "agents.role_groups.engineer.enable_tool_tags",
        ),
        (
            "agents.roleGroups.engineer.enableToolGroups",
            "agents.role_groups.engineer.enable_tool_groups",
        ),
        (
            "agents.roleGroups.engineer.disableToolGroups",
            "agents.role_groups.engineer.disable_tool_groups",
        ),
        (
            "agents.roleGroups.engineer.enableTools",
            "agents.role_groups.engineer.enable_tools",
        ),
        (
            "agents.roleGroups.engineer.disableTools",
            "agents.role_groups.engineer.disable_tools",
        ),
        (
            "agents.roleGroups.engineer.requiredSkills",
            "agents.role_groups.engineer.required_skills",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.enabled",
            "agents.role_groups.engineer.roles.engineer.enable",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.interSessionReceiver",
            "agents.role_groups.engineer.roles.engineer.inter_session_receiver",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.interSessionAutoStart",
            "agents.role_groups.engineer.roles.engineer.inter_session_auto_start",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.thinkingSummary",
            "agents.role_groups.engineer.roles.engineer.thinking_summary",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.serviceTier",
            "agents.role_groups.engineer.roles.engineer.service_tier",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.promptFragments",
            "agents.role_groups.engineer.roles.engineer.prompt_fragments",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.promptOverride",
            "agents.role_groups.engineer.roles.engineer.prompt_override",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.disableToolTags",
            "agents.role_groups.engineer.roles.engineer.disable_tool_tags",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.enableToolTags",
            "agents.role_groups.engineer.roles.engineer.enable_tool_tags",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.enableToolGroups",
            "agents.role_groups.engineer.roles.engineer.enable_tool_groups",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.disableToolGroups",
            "agents.role_groups.engineer.roles.engineer.disable_tool_groups",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.enableTools",
            "agents.role_groups.engineer.roles.engineer.enable_tools",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.disableTools",
            "agents.role_groups.engineer.roles.engineer.disable_tools",
        ),
        (
            "agents.roleGroups.engineer.roles.engineer.requiredSkills",
            "agents.role_groups.engineer.roles.engineer.required_skills",
        ),
    ];

    for (legacy, canonical) in cases {
        assert_eq!(normalize_harness_config_override_key(legacy), canonical);
    }
}

/// Ensures role-level `enabled`/`enable` conflicts are rejected with path
/// context.
#[test]
fn harness_rejects_same_layer_role_alias_conflict() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
                engineer:
                  roles:
                    engineer:
                      enabled: false
                      enable: true
        "#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflicting aliases");

    assert!(
        error.to_string().contains("enabled")
            && error.to_string().contains("enable")
            && error.to_string().contains("engineer"),
        "unexpected error: {error}"
    );
}

#[cfg(unix)]
/// Ensures unreadable drop-in directory discovery errors are reported instead
/// of skipped.
#[test]
fn unreadable_drop_in_directory_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), "greeting: false\n").expect("write base");
    let drop_dir = dir.join("cli.d");
    std::fs::create_dir_all(&drop_dir).expect("mkdir dropins");
    std::fs::set_permissions(&drop_dir, path_std_fs::Permissions::from_mode(0o000))
        .expect("chmod unreadable");

    let error = load_cli_settings_in(&dirs_with_config(dir)).expect_err("unreadable drop-in dir");

    std::fs::set_permissions(&drop_dir, path_std_fs::Permissions::from_mode(0o700))
        .expect("restore permissions");
    assert!(
        error.to_string().contains("failed to read"),
        "unexpected error: {error}"
    );
}

/// Ensures an existing drop-in path must be a directory, not a file or symlink
/// target.
#[test]
fn cli_drop_in_path_must_be_a_directory() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), "greeting: false\n").expect("write base");
    std::fs::write(dir.join("cli.d"), "not a directory\n").expect("write file");

    let error = load_cli_settings_in(&dirs_with_config(dir)).expect_err("file cli.d should fail");

    assert!(
        error.to_string().contains("not a directory"),
        "unexpected error: {error}"
    );
}

/// Ensures an existing drop-in path must be a directory, not a file or symlink
/// target.
#[test]
fn harness_drop_in_path_must_be_a_directory() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), "default_role: manager\n").expect("write base");
    std::fs::write(dir.join("harness.d"), "not a directory\n").expect("write file");

    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("file harness.d should fail");

    assert!(
        error.to_string().contains("not a directory"),
        "unexpected error: {error}"
    );
}

/// Ensures the state loader falls back to CLI config defaults when no state
/// file exists.
#[test]
fn cli_state_defaults_to_cli_config_when_state_file_is_missing() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("cli.yaml"),
        r#"{ show_diff: true, show_thinking: false, show_turn_stats: true, redraw_counter: true, redraw_history_size: 321, show_ui_io: true, show_tools: "compact", show_messages: "self-full", notice_level: "warning", show_status: "minimal", show_prompt_scroll_indicator: false }"#,
    )
    .expect("write");

    let dirs = dirs_with_config_and_state(&config_dir, &state_dir);
    let settings = load_cli_settings_in(&dirs).expect("load settings");
    let state = CliState::load_with_default(&dirs, settings.default_state());

    assert_eq!(
        state,
        CliState {
            show_diff: true,
            show_thinking: false,
            show_turn_stats: true,
            redraw_counter: true,
            redraw_history_size: 321,
            show_ui_io: true,
            show_tools: ShowTools::Compact,
            show_messages: ShowMessages::SelfFull,
            notice_level: tau_proto::NoticeLevel::Warning,
            show_status: ShowStatus::Minimal,
            show_prompt_scroll_indicator: false,
        }
    );
}

/// Existing `cli.json` files from older Tau versions may not mention newer
/// settings. Missing persisted fields must keep the caller-supplied CLI config
/// defaults instead of falling back to `CliState::default()`.
#[test]
fn partial_cli_state_overlays_cli_config_defaults() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(state_dir.join("cli.json"), r#"{"show_diff":false}"#).expect("write state");
    let dirs = TauDirs {
        config_dir: None,
        state_dir: Some(state_dir),
    };
    let default = CliState {
        show_diff: true,
        redraw_history_size: 321,
        ..CliState::default()
    };

    let state = CliState::load_with_default(&dirs, default);

    assert!(!state.show_diff);
    assert_eq!(state.redraw_history_size, 321);
}

/// Ensures persisted state values override CLI config defaults where state is
/// authoritative.
#[test]
fn cli_state_file_overrides_cli_config_defaults() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(config_dir.join("cli.yaml"), r#"{ show_thinking: false }"#).expect("write");
    std::fs::write(state_dir.join("cli.json"), r#"{"show_thinking":true}"#).expect("write");

    let dirs = dirs_with_config_and_state(&config_dir, &state_dir);
    let settings = load_cli_settings_in(&dirs).expect("load settings");
    let state = CliState::load_with_default(&dirs, settings.default_state());

    assert!(state.show_thinking);
}

/// Ensures user harness.yaml values override the built-in baseline config.
#[test]
fn harness_settings_user_override_wins_over_built_in() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
                session_retention_days: 7,
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.session_retention_days, 7);
    assert_eq!(
        s.session_retention(),
        Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
    );
}

/// Ensures user config can override the agent id template field.
#[test]
fn harness_settings_accept_agent_id_template_in_user_config() {
    // The role-override merge pass rereads harness.yaml with a narrower wire
    // type. It must ignore top-level agent settings rather than reject configs
    // that are valid for the main harness settings layer.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                id_template: "{{role}}-{{random_alphanumeric 4}}",
                display_name_template: "{{role_group}} {{task_name}}",
            },
        }"#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        settings.agent_id_template,
        "{{role}}-{{random_alphanumeric 4}}"
    );
    assert_eq!(
        settings.agent_display_name_template.as_deref(),
        Some("{{role_group}} {{task_name}}")
    );
}

/// Ensures legacy camelCase keys in higher-precedence config override built-in
/// snake_case keys.
#[test]
fn harness_settings_accept_legacy_camel_case_overrides_over_snake_case_builtins() {
    // Built-in defaults now use snake_case. Legacy user layers still need to
    // override them instead of becoming duplicate alias fields after layering.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                defaultRole: "manager",
                idTemplate: "legacy-{{random_alphanumeric 4}}",
                roleGroups: {
                    engineer: {
                        promptFragments: [{ name: "legacy.group", priority: 80, text: "group" }],
                        roles: {
                            "engineer": {
                                enableTools: ["web_search"],
                                promptFragments: [{ name: "legacy.role", priority: 90, text: "role" }],
                            },
                        },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(settings.default_role.as_deref(), Some("manager"));
    assert_eq!(
        settings.agent_id_template,
        "legacy-{{random_alphanumeric 4}}"
    );
    let engineer = settings.roles.get("engineer").expect("engineer role");
    assert!(
        engineer
            .enable_tools
            .iter()
            .any(|tool| tool.as_str() == "web_search")
    );
    assert!(
        engineer
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name.as_str() == "legacy.group")
    );
    assert!(
        engineer
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name.as_str() == "legacy.role")
    );
}

/// Ensures CLI config overrides parse as YAML and layer after config files.
#[test]
fn harness_config_cli_overrides_are_applied_last_and_typed() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            session_retention_days: 7,
            diagnostic_retention_days: 9,
            extensions: {
                "core-shell": { config: { working_directory: "/from-file" } },
                "std-websearch": { enable: true },
            },
        }"#,
    )
    .expect("write");

    let file_settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load file layer");
    assert_eq!(file_settings.session_retention_days, 7);
    assert_eq!(file_settings.diagnostic_retention_days, 9);

    let overrides = [
        HarnessConfigCliOverride::from_str("session_retention_days=3").expect("override"),
        HarnessConfigCliOverride::from_str("diagnostic_retention_days=0")
            .expect("diagnostic override"),
        HarnessConfigCliOverride::from_str(
            "extensions.core-shell.config.working_directory=/from-cli",
        )
        .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.std-websearch.enable=false")
            .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.core-shell.command=[\"tau\", \"ext\"]")
            .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.core-shell.toolPrefix=work")
            .expect("override"),
    ];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(s.session_retention_days, 3);
    assert_eq!(s.diagnostic_retention_days, 0);
    assert_eq!(s.diagnostic_retention(), None);
    let core_shell = &s.extensions["core-shell"];
    assert_eq!(
        core_shell.config.as_ref().and_then(|config| {
            config
                .get("working_directory")
                .and_then(serde_json::Value::as_str)
        }),
        Some("/from-cli")
    );
    assert_eq!(
        core_shell.command.as_ref().expect("command"),
        &vec!["tau".to_owned(), "ext".to_owned()]
    );
    assert_eq!(
        core_shell
            .tool_prefix
            .as_ref()
            .and_then(Option::as_ref)
            .map(tau_proto::ToolNamePrefix::as_str),
        Some("work")
    );
    assert_eq!(s.extensions["std-websearch"].enable, Some(false));
}

#[test]
fn harness_settings_extension_require_parses_and_cli_overrides() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            extensions: {
                "core-shell": { require: false },
                "std-websearch": { enable: true },
            },
        }"#,
    )
    .expect("write");

    let overrides = [
        HarnessConfigCliOverride::from_str("extensions.std-websearch.require=false")
            .expect("override"),
    ];
    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert_eq!(settings.extensions["core-shell"].require, Some(false));
    assert_eq!(settings.extensions["std-websearch"].require, Some(false));
    let no_cli = load_harness_settings_in(&dirs_with_config(dir)).expect("load without cli");
    assert_eq!(no_cli.extensions["std-websearch"].require, None);
}

#[test]
fn harness_settings_extension_require_rejects_wrong_type() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{ extensions: { "core-shell": { require: "sometimes" } } }"#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("wrong require type should fail");

    assert!(
        error.to_string().contains("require") || error.to_string().contains("bool"),
        "unexpected error: {error}"
    );
}

/// Ensures `--harness-config` can update nested role settings at highest
/// precedence.
#[test]
fn harness_config_cli_overrides_can_update_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "agents.role_groups.engineer.roles.engineer.effort=low",
    )
    .expect("override")];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(s.roles["engineer"].effort, Some(tau_proto::Effort::Low));
}

/// Ensures malformed CLI config overrides fail explicitly at parse time.
#[test]
fn harness_config_cli_overrides_reject_bad_key_value() {
    assert!(HarnessConfigCliOverride::from_str("missing-equals").is_err());
    assert!(HarnessConfigCliOverride::from_str("=value").is_err());
}

/// Ensures CLI overrides using legacy role aliases still target canonical role
/// fields.
#[test]
fn harness_config_cli_overrides_normalize_legacy_role_aliases() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "agents.role_groups.engineer.roles.engineer.enabled=false",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert!(!settings.roles.contains_key("engineer"));
}

/// Ensures CLI overrides using legacy nested `agents.roleGroups` aliases still
/// target canonical role settings.
#[test]
fn harness_config_cli_overrides_normalize_legacy_agents_role_aliases() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "agents.roleGroups.engineer.roles.engineer.effort=low",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert_eq!(
        settings.roles["engineer"].effort,
        Some(tau_proto::Effort::Low)
    );
}

/// Ensures CLI overrides reject alias/canonical conflicts within the same
/// synthetic layer.
#[test]
fn harness_config_cli_overrides_reject_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    let overrides = [
        HarnessConfigCliOverride::from_str("agents.defaultRole=manager").expect("legacy override"),
        HarnessConfigCliOverride::from_str("agents.default_role=engineer")
            .expect("canonical override"),
    ];

    let error =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(td.path()), &[], &overrides)
            .expect_err("conflicting overrides");

    assert!(
        error.to_string().contains("defaultRole") && error.to_string().contains("default_role"),
        "unexpected error: {error}"
    );
}

/// Ensures YAML map-valued CLI overrides normalize aliases inside the supplied
/// value.
#[test]
fn harness_config_cli_overrides_normalize_map_value_aliases() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "agents.role_groups.engineer.roles.engineer={enabled: false}",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert!(!settings.roles.contains_key("engineer"));
}

/// Ensures YAML map-valued CLI overrides reject alias/canonical conflicts
/// inside the value.
#[test]
fn harness_config_cli_overrides_reject_map_value_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "agents.role_groups.engineer.roles.engineer={enabled: false, enable: true}",
    )
    .expect("override")];

    let error =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect_err("conflicting map aliases");

    assert!(
        error.to_string().contains("enabled")
            && error.to_string().contains("enable")
            && error.to_string().contains("engineer"),
        "unexpected error: {error}"
    );
}

/// Ensures map-valued CLI overrides can address dotted tool-policy rule names
/// and normalize `enabled` inside the supplied map.
#[test]
fn harness_config_cli_map_override_disables_tool_policy_with_enabled_alias() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        r#"tool_policy={rules: {builtin.chatgpt-shell: {enabled: false}}}"#,
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert!(!settings.tool_policy.rules["builtin.chatgpt-shell"].enable);
}

/// Ensures dotted CLI overrides normalize tool policy rule aliases before
/// duplicate detection, matching file-layer and whole-map override behavior.
#[test]
fn harness_config_cli_overrides_reject_tool_policy_rule_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [
        HarnessConfigCliOverride::from_str("tool_policy.rules.local.enabled=false")
            .expect("legacy override"),
        HarnessConfigCliOverride::from_str("tool_policy.rules.local.enable=true")
            .expect("canonical override"),
    ];

    let error =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect_err("conflicting tool policy aliases");

    assert!(
        error.to_string().contains("enabled") && error.to_string().contains("enable"),
        "unexpected error: {error}"
    );
}

/// Ensures role tool allow/deny lists load into effective role settings.
#[test]
fn harness_settings_load_role_tool_lists() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    roles: {
                        engineer: {
                            tools: ["read", "grep"],
                            disableToolTags: ["shell:*"],
                            enableToolTags: ["shell:cd"],
                            disableToolGroups: ["shell"],
                            enableToolGroups: ["search"],
                            disableTools: ["grep"],
                            enableTools: ["web_search"],
                        },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.roles["engineer"].tools.as_ref().expect("tools"),
        &vec![
            tau_proto::ToolName::new("read"),
            tau_proto::ToolName::new("grep")
        ]
    );
    assert!(
        s.roles["engineer"].disable_tool_tags[0].matches(&tau_proto::ToolTag::new("shell:read"))
    );
    assert!(s.roles["engineer"].enable_tool_tags[0].matches(&tau_proto::ToolTag::new("shell:cd")));
    assert_eq!(
        s.roles["engineer"].disable_tool_groups,
        vec![tau_proto::ToolGroupName::new("shell")]
    );
    assert_eq!(
        s.roles["engineer"].enable_tool_groups,
        vec![tau_proto::ToolGroupName::new("search")]
    );
    assert_eq!(
        s.roles["engineer"].enable_tools,
        vec![tau_proto::ToolName::new("web_search")]
    );
    assert_eq!(
        s.roles["engineer"].disable_tools,
        vec![tau_proto::ToolName::new("grep")]
    );
}

/// Ensures higher-precedence role drop-ins can clear inherited scalar fields
/// and tool lists.
#[test]
fn harness_role_drop_in_can_clear_inherited_scalar_and_tool_lists() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  enable: false
                  description: Base description
                  model: openai/gpt-5
                  compaction: disabled
                  prompt_override: built-in
                  tools: [read]
                  enable_tools: [grep]
                  disable_tools: [shell]
        "#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d/10-clear.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  enable: null
                  description: null
                  model: null
                  compaction: null
                  prompt_override: null
                  tools: null
                  enable_tools: []
                  disable_tools: []
        "#,
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = settings.roles.get("reviewer").expect("reviewer role");

    assert_eq!(reviewer.enable, None);
    assert_eq!(reviewer.description, None);
    assert_eq!(reviewer.model, None);
    assert_eq!(reviewer.compaction, None);
    assert_eq!(reviewer.prompt_override, None);
    assert_eq!(reviewer.tools, None);
    assert!(reviewer.enable_tools.is_empty());
    assert!(reviewer.disable_tools.is_empty());
}

/// Ensures narrower role fields remain effective over broader group clears from
/// a later layer.
#[test]
fn harness_role_overrides_precede_later_group_clears() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  description: Base description
                  prompt_override: built-in
        "#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d/10-group-clear.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              description: null
              prompt_override: null
        "#,
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = settings.roles.get("reviewer").expect("reviewer role");

    assert_eq!(reviewer.description.as_deref(), Some("Base description"));
    assert_eq!(reviewer.prompt_override.as_deref(), Some("built-in"));
}

/// Ensures group defaults apply to inherited group members even when the layer
/// also adds a role.
#[test]
fn harness_role_group_defaults_apply_to_existing_roles_when_adding_role() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            engineer:
              disable_tools: [shell]
              roles:
                custom: {}
        "#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(
        settings.roles["engineer"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
    );
    assert_eq!(
        settings.roles["custom"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
    );
}

/// Ensures required role skills are parsed from snake_case and camelCase,
/// inherited from role groups, and de-duplicated so duplicate group/role
/// requirements do not produce noisy repeated diagnostics later.
#[test]
fn harness_role_required_skills_are_additive_and_deduped() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          required_skills: [global-skill]
          role_groups:
            engineer:
              required_skills: [group-skill, shared-skill]
              roles:
                reviewer:
                  requiredSkills: [role-skill, shared-skill]
                implementer: {}
        "#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(
        settings.roles["reviewer"].required_skills,
        vec![
            tau_proto::SkillName::from("group-skill"),
            tau_proto::SkillName::from("shared-skill"),
            tau_proto::SkillName::from("role-skill"),
            tau_proto::SkillName::from("global-skill"),
        ]
    );
    assert_eq!(
        settings.roles["implementer"].required_skills,
        vec![
            tau_proto::SkillName::from("group-skill"),
            tau_proto::SkillName::from("shared-skill"),
            tau_proto::SkillName::from("global-skill"),
        ]
    );
}

/// Ensures higher-precedence layers add required skills instead of replacing
/// lower-precedence requirements. Required skills are fail-closed role
/// prerequisites, so partial overrides must not accidentally erase them.
#[test]
fn harness_role_required_skills_accumulate_across_layers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  required_skills: [base-skill]
        "#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d/10-extra.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              required_skills: [group-extra]
              roles:
                reviewer:
                  required_skills: [role-extra, base-skill]
        "#,
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(
        settings.roles["reviewer"].required_skills,
        vec![
            tau_proto::SkillName::from("group-extra"),
            tau_proto::SkillName::from("base-skill"),
            tau_proto::SkillName::from("role-extra"),
        ]
    );
}

/// Ensures role-specific compaction settings load and merge correctly.
#[test]
fn harness_settings_load_role_compaction() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    compaction: { threshold: 70000 },
                    roles: {
                        engineer: { compaction: { threshold: 80000 } },
                        reviewer: {},
                        disabled: { compaction: "disabled" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.roles["engineer"].compaction,
        Some(RoleCompaction::Threshold(80000))
    );
    assert_eq!(
        s.roles["reviewer"].compaction,
        Some(RoleCompaction::Threshold(70000))
    );
    assert_eq!(
        s.roles["disabled"].compaction,
        Some(RoleCompaction::Disabled)
    );
}

/// Ensures named context-size alerts merge field-by-field from agent globals
/// through group defaults and role overrides, including default enablement and
/// the default compaction reminder.
#[test]
fn harness_settings_merge_named_context_size_alerts() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          context_size_alerts:
            compact-soon:
              threshold: 160000
            final-warning:
              threshold: 190000
              message: Finish immediately.
            default-message:
              threshold: 120000
          role_groups:
            custom:
              context_size_alerts:
                compact-soon:
                  message: Compact after this task.
              roles:
                reviewer:
                  context_size_alerts:
                    compact-soon:
                      enable: false
                implementer: {}
        "#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = &settings.roles["reviewer"].context_size_alerts;
    assert_eq!(reviewer["compact-soon"].threshold, 160_000);
    assert!(!reviewer["compact-soon"].enable);
    assert_eq!(reviewer["compact-soon"].message, "Compact after this task.");
    assert_eq!(reviewer["final-warning"].message, "Finish immediately.");

    let implementer = &settings.roles["implementer"].context_size_alerts;
    assert!(implementer["compact-soon"].enable);
    assert_eq!(
        implementer["compact-soon"].message,
        "Compact after this task."
    );
    assert_eq!(implementer["final-warning"].message, "Finish immediately.");
    assert_eq!(
        implementer["default-message"].message,
        DEFAULT_CONTEXT_SIZE_ALERT_MESSAGE
    );
}

/// Ensures legacy and canonical alert-map spellings normalize before file-layer
/// merging, so a drop-in can disable an inherited camel-case role alert.
#[test]
fn harness_settings_merge_context_size_alert_alias_across_layers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          roleGroups:
            custom:
              roles:
                reviewer:
                  contextSizeAlerts:
                    compact-soon:
                      threshold: 160000
        "#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d/10-disable.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                reviewer:
                  context_size_alerts:
                    compact-soon:
                      enable: false
        "#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let alert = &settings.roles["reviewer"].context_size_alerts["compact-soon"];
    assert_eq!(alert.threshold, 160_000);
    assert!(!alert.enable);
}

/// Ensures a newly declared named alert cannot omit the threshold that defines
/// when it fires, even when the entry only attempts to disable itself.
#[test]
fn harness_settings_reject_context_size_alert_without_threshold() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
        agents:
          context_size_alerts:
            incomplete:
              enable: false
        "#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("missing threshold must fail");
    assert!(error.to_string().contains("requires a positive threshold"));
}

/// Ensures an explicitly empty internal-prompt message is rejected instead of
/// creating a context alert that silently activates the model.
#[test]
fn harness_settings_reject_empty_context_size_alert_message() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
        agents:
          context_size_alerts:
            silent:
              threshold: 100
              message: ""
        "#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("empty message must fail");
    assert!(error.to_string().contains("message must not be empty"));
}

/// Ensures group-level tool defaults update inherited roles without relisting
/// each role.
#[test]
fn harness_settings_load_role_group_default_tool_overrides_without_relisting_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: { enable_tools: ["email_list_recent"], disable_tools: ["email"] },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    for role_name in ["engineer", "engineer-junior", "engineer-senior"] {
        assert_eq!(
            s.roles[role_name].enable_tools,
            vec![tau_proto::ToolName::new("email_list_recent")]
        );
        assert_eq!(
            s.roles[role_name].disable_tools,
            vec![tau_proto::ToolName::new("email")]
        );
    }
}

/// Ensures roles can opt into a disabled extension tool group through either
/// their group defaults or their own narrow role settings.
#[test]
fn harness_roles_can_opt_into_the_swarm_tool_group() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    swarm-team:
      enable_tool_groups: [swarm]
      roles:
        group-member: {}
    swarm-role:
      roles:
        role-member:
          enable_tool_groups: [swarm]
"#,
    )
    .expect("write config");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    for role in ["group-member", "role-member"] {
        assert_eq!(
            settings.roles[role].enable_tool_groups,
            vec![tau_proto::ToolGroupName::new("swarm")]
        );
    }
}

/// Ensures user config may define a new role group with its own roles.
#[test]
fn harness_settings_allow_new_role_group() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                reviewers: {
                    disable_tools: ["email"],
                    roles: {
                        reviewer: { effort: "high" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.role_groups.last().expect("new group").name, "reviewers");
    assert_eq!(
        s.roles["reviewer"].disable_tools,
        vec![tau_proto::ToolName::new("email")]
    );
}

/// Ensures a role name cannot appear in multiple groups, avoiding ambiguous
/// defaults.
#[test]
fn harness_settings_rejects_role_in_multiple_groups() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                reviewers: {
                    roles: {
                        engineer: { effort: "high" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject duplicate role");
    assert!(
        error
            .to_string()
            .contains("role `engineer` appears in multiple role_groups"),
        "error should mention duplicate role: {error}"
    );
}

/// Ensures unknown top-level harness.yaml fields fail instead of being ignored.
#[test]
fn harness_settings_rejects_unknown_top_level_fields() {
    // Unknown harness.yaml keys used to be silently ignored. That hides stale
    // configs after refactors, so loading must fail and let the harness print a
    // loud startup warning instead.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), r#"{ staleThing: true }"#).expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject unknown field");
    assert!(
        error.to_string().contains("staleThing"),
        "error should mention unknown field: {error}"
    );
}

/// Ensures pre-`agents` role-setting locations are rejected instead of silently
/// accepted after the schema move.
#[test]
fn harness_settings_rejects_root_agent_role_settings() {
    let file_cases = [
        ("default_role", r#"{ default_role: stale }"#),
        ("defaultRole", r#"{ defaultRole: stale }"#),
        (
            "role_groups",
            r#"{ role_groups: { stale: { roles: { stale: {} } } } }"#,
        ),
        (
            "roleGroups",
            r#"{ roleGroups: { stale: { roles: { stale: {} } } } }"#,
        ),
        (
            "prompt_fragments",
            r#"{ prompt_fragments: [{ name: stale, priority: 1, text: stale }] }"#,
        ),
        (
            "promptFragments",
            r#"{ promptFragments: [{ name: stale, priority: 1, text: stale }] }"#,
        ),
        ("required_skills", r#"{ required_skills: [stale] }"#),
        ("requiredSkills", r#"{ requiredSkills: [stale] }"#),
    ];
    for (key, yaml) in file_cases {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("harness.yaml"), yaml).expect("write");

        let error = load_harness_settings_in(&dirs_with_config(dir))
            .expect_err("reject misplaced agent role config");
        assert!(
            error.to_string().contains(key),
            "error should mention misplaced {key}: {error}"
        );
    }

    let cli_cases = [
        "default_role=stale",
        "defaultRole=stale",
        "role_groups={stale: {roles: {stale: {}}}}",
        "roleGroups={stale: {roles: {stale: {}}}}",
        "prompt_fragments=[{ name: stale, priority: 1, text: stale }]",
        "promptFragments=[{ name: stale, priority: 1, text: stale }]",
        "required_skills=[stale]",
        "requiredSkills=[stale]",
    ];
    for override_text in cli_cases {
        let td = TempDir::new().expect("tempdir");
        let override_ = HarnessConfigCliOverride::from_str(override_text).expect("override");

        let error = load_harness_settings_with_cli_overrides_in(
            &dirs_with_config(td.path()),
            &[],
            &[override_],
        )
        .expect_err("reject misplaced agent role CLI override");
        let key = override_text.split('=').next().expect("key");
        assert!(
            error.to_string().contains(key),
            "error should mention misplaced {key}: {error}"
        );
    }
}

/// Ensures unknown role fields fail so role-setting typos are visible.
#[test]
fn harness_settings_rejects_unknown_role_fields() {
    // Role entries are nested under arbitrary group and role names, so strict
    // parsing has to happen at the AgentRole level too.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { staleRoleField: true },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject unknown role field");
    assert!(
        error.to_string().contains("staleRoleField"),
        "error should mention unknown role field: {error}"
    );
}

/// Ensures unknown prompt-fragment fields fail so prompt config typos are
/// visible.
#[test]
fn harness_settings_rejects_unknown_prompt_fragment_fields() {
    // Prompt fragments are user-authored config too; typos there must not be
    // accepted as no-ops.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                prompt_fragments: [
                { name: "global.typo", priority: 50, text: "x", staleFragmentField: true },
            ],
            },
        }"#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("reject unknown fragment field");
    assert!(
        error.to_string().contains("staleFragmentField"),
        "error should mention unknown fragment field: {error}"
    );
}

/// Ensures role CLI overrides are applied after config files and later
/// overrides win.
#[test]
fn harness_settings_role_cli_overrides_apply_in_order_after_config() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                manager: {
                    roles: {
                        manager: { enable: false },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[
            RoleCliOverride::DisableAll,
            RoleCliOverride::Enable("manager".to_owned()),
        ],
    )
    .expect("load");

    assert_eq!(s.roles.keys().collect::<Vec<_>>(), vec!["manager"]);
    assert_eq!(s.role_groups.len(), 1);
    assert_eq!(s.role_groups[0].name, "manager");
    assert_eq!(s.role_groups[0].roles, vec!["manager"]);
}

/// Ensures later CLI role overrides can disable a role set by earlier
/// overrides.
#[test]
fn harness_settings_role_cli_overrides_later_disable_wins() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[
            RoleCliOverride::Enable("engineer-senior".to_owned()),
            RoleCliOverride::Disable("engineer-senior".to_owned()),
        ],
    )
    .expect("load");

    assert!(!s.roles.contains_key("engineer-senior"));
}

/// Ensures CLI overrides can disable every role and produce an empty effective
/// role set.
#[test]
fn harness_settings_role_cli_disable_all_leaves_no_effective_roles() {
    // `--disable-roles-all` must not be undone by default-role fallback. The
    // harness reports an explicit startup error for this empty effective role set.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    let s = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[RoleCliOverride::DisableAll],
    )
    .expect("load");

    assert!(s.roles.is_empty());
    assert!(s.role_groups.is_empty());
    assert_eq!(s.default_role.as_deref(), Some("engineer"));
}

/// Ensures CLI overrides for unknown role paths fail with explicit config
/// errors.
#[test]
fn harness_settings_role_cli_unknown_role_errors() {
    // CLI role typos must fail startup instead of silently leaving the effective
    // role set unchanged.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    let error = load_harness_settings_with_role_overrides_in(
        &dirs_with_config(dir),
        &[RoleCliOverride::Enable("missing".to_owned())],
    )
    .expect_err("unknown role should fail");

    assert!(matches!(
        error,
        SettingsError::UnknownRoleCliOverride(role) if role == "missing"
    ));
}

/// Ensures harness.d drop-ins layer on top of the base harness.yaml file.
#[test]
fn cli_settings_drop_in_layers_on_top_of_base() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), r#"{ greeting: true }"#).expect("write");
    std::fs::create_dir(dir.join("cli.d")).expect("mkdir");
    std::fs::write(
        dir.join("cli.d").join("01-override.yaml"),
        r#"{ greeting: false }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
}

/// Ensures domain-specific drop-in layers merge with the same precedence rules
/// as base config.
#[test]
fn harness_drop_in_layers_merge_through_domain_overrides() {
    // Harness files are applied as sparse overrides one layer at a time. This
    // keeps role prompt fragments additive across the built-in baseline,
    // harness.yaml, and harness.d/*.yaml instead of letting generic YAML array
    // replacement discard earlier fragments before role merging can run.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            session_retention_days: 7,
            extensions: {
                mything: { command: ["mything"] },
            },
            agents: {
                prompt_fragments: [
                { name: "global.local", priority: 60, text: "Local global instruction." },
            ],
                role_groups: {
                manager: {
                    roles: {
                        "project-manager": { prompt_fragments: [{ name: "manager.local", priority: 170, text: "Local manager instruction." }] },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("mkdir harness.d");
    std::fs::write(
        dir.join("harness.d").join("01-extra.yaml"),
        r#"{
            session_retention_days: 14,
            extensions: {
                mything: { suffix: ["--flag"] },
            },
            agents: {
                prompt_fragments: [
                { name: "global.drop-in", priority: 70, text: "Drop-in global instruction." },
            ],
                role_groups: {
                manager: {
                    roles: {
                        "project-manager": { prompt_fragments: [{ name: "manager.drop-in", priority: 180, text: "Drop-in manager instruction." }] },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.session_retention_days, 14);
    assert_eq!(
        s.extensions["mything"].command.as_ref().expect("command"),
        &vec!["mything".to_owned()]
    );
    assert_eq!(
        s.extensions["mything"].suffix.as_ref().expect("suffix"),
        &vec!["--flag".to_owned()]
    );
    assert!(
        s.prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local global instruction.")
    );
    assert!(
        s.prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in global instruction.")
    );
    let manager = &s.roles["project-manager"];
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local manager instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in manager instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Local global instruction.")
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Drop-in global instruction.")
    );
}

/// Ensures agent-global prompt fragments are appended to every effective role
/// prompt.
#[test]
fn harness_global_prompt_fragments_apply_to_all_roles() {
    // `agents.prompt_fragments` are role-independent style/context hooks. They
    // must apply to built-in roles and roles created by user config without
    // duplicating the same fragment when a drop-in repeats it exactly.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                prompt_fragments: [
                { name: "global.simple", priority: 65, text: "Use simple words." },
            ],
                role_groups: {
                custom: {
                    roles: {
                        custom: { model: "openai/custom" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("mkdir harness.d");
    std::fs::write(
        dir.join("harness.d").join("01-repeat.yaml"),
        r#"{
            agents: {
                prompt_fragments: [
                { name: "global.simple", priority: 65, text: "Use simple words." },
            ],
            },
        }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.prompt_fragments
            .iter()
            .filter(|fragment| fragment.name == "global.simple")
            .count(),
        1
    );
    for role_name in ["engineer", "custom"] {
        let role = &s.roles[role_name];
        assert_eq!(
            role.prompt_fragments
                .iter()
                .filter(|fragment| fragment.name == "global.simple")
                .count(),
            1,
            "global fragment should apply once to {role_name}"
        );
    }
}

/// Ensures top-level agents provider settings become effective defaults for
/// every role, including all supported model-facing provider parameters.
#[test]
fn harness_agent_provider_defaults_apply_to_all_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          model: openai/global-model
          effort: medium
          verbosity: high
          thinkingSummary: concise
          serviceTier: flex
          compaction: { threshold: 123456 }
          role_groups:
            custom:
              roles:
                inherited: {}
        "#,
    )
    .expect("write harness");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let inherited = &settings.roles["inherited"];
    assert_eq!(
        inherited.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/global-model")
    );
    assert_eq!(inherited.effort, Some(tau_proto::Effort::Medium));
    assert_eq!(inherited.verbosity, Some(tau_proto::Verbosity::High));
    assert_eq!(
        inherited.thinking_summary,
        Some(tau_proto::ThinkingSummary::Concise)
    );
    assert_eq!(inherited.service_tier, Some(tau_proto::ServiceTier::Flex));
    assert_eq!(
        inherited.compaction,
        Some(RoleCompaction::Threshold(123_456))
    );
}

/// Ensures visibility defaults to true and follows normal broad-to-specific
/// role inheritance without disabling roles.
#[test]
fn harness_role_visibility_defaults_and_inherits() {
    let td = TempDir::new().expect("tempdir");
    let default_settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load defaults");
    assert_eq!(default_settings.roles["engineer"].visible, Some(true));

    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          visible: false
          role_groups:
            hidden:
              roles:
                inherited: {}
                role-visible:
                  visible: true
            visible:
              visible: true
              roles:
                group-inherited: {}
                role-hidden:
                  visible: false
        "#,
    )
    .expect("write harness");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(settings.roles["inherited"].visible, Some(false));
    assert_eq!(settings.roles["role-visible"].visible, Some(true));
    assert_eq!(settings.roles["group-inherited"].visible, Some(true));
    assert_eq!(settings.roles["role-hidden"].visible, Some(false));
}

/// Locks scope precedence across source layers: all agent defaults resolve
/// first, then all group defaults, and role overrides resolve last.
#[test]
fn harness_agent_provider_defaults_precede_groups_and_roles_across_layers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          model: openai/base-global
          effort: minimal
          verbosity: high
          thinking_summary: detailed
          service_tier: fast
          role_groups:
            custom:
              model: openai/base-group
              effort: low
              roles:
                reviewer:
                  model: openai/base-role
                  effort: high
                  service_tier: fast
        "#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir drop-ins");
    std::fs::write(
        dir.join("harness.d/10-provider-defaults.yaml"),
        r#"
        agents:
          model: openai/drop-in-global
          effort: off
          thinking_summary: null
          service_tier: flex
          role_groups:
            custom:
              effort: medium
              roles:
                reviewer:
                  model: openai/drop-in-role
                newcomer: {}
        "#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = &settings.roles["reviewer"];
    assert_eq!(
        reviewer.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/drop-in-role")
    );
    assert_eq!(reviewer.effort, Some(tau_proto::Effort::High));
    assert_eq!(reviewer.thinking_summary, None);
    assert_eq!(reviewer.service_tier, Some(tau_proto::ServiceTier::Fast));
    assert_eq!(
        settings.roles["newcomer"].verbosity,
        Some(tau_proto::Verbosity::High)
    );
}

/// Ensures relative provider settings resolve broadly to narrowly, use the
/// documented built-in bases when needed, and saturate at each setting's ends.
#[test]
fn harness_relative_provider_settings_merge_and_saturate() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          effort: increase
          verbosity: decrease:99
          thinking_summary: decrease
          role_groups:
            custom:
              effort: increase:99
              thinking_summary: increase:2
              roles:
                reviewer:
                  effort: decrease:99
                  verbosity: increase
                  thinking_summary: increase:99
        "#,
    )
    .expect("write harness");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = &settings.roles["reviewer"];
    assert_eq!(reviewer.effort, Some(tau_proto::Effort::Off));
    assert_eq!(reviewer.verbosity, Some(tau_proto::Verbosity::Medium));
    assert_eq!(
        reviewer.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );
}

/// Ensures a role-relative patch from an earlier file resolves after a later
/// group default rather than being overwritten by that broader setting.
#[test]
fn harness_role_overrides_precede_later_group_defaults_across_layers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
agents:
  role_groups:
    precedence:
      verbosity: low
      roles:
        precedence-junior:
          effort: decrease
        precedence:
          enable: true
"#,
    )
    .expect("write base");
    std::fs::create_dir(dir.join("harness.d")).expect("create drop-ins");
    std::fs::write(
        dir.join("harness.d/20-engineer-defaults.yaml"),
        r#"
agents:
  role_groups:
    precedence:
      model: chatgpt/gpt-5.6-terra
      effort: high
      roles:
        precedence-senior:
          model: chatgpt/gpt-5.6-sol
          effort: medium
"#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        settings.roles["precedence-junior"].effort,
        Some(tau_proto::Effort::Medium)
    );
    assert_eq!(
        settings.roles["precedence"].effort,
        Some(tau_proto::Effort::High)
    );
    assert_eq!(
        settings.roles["precedence-junior"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("chatgpt/gpt-5.6-terra")
    );
    assert_eq!(
        settings.roles["precedence-senior"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("chatgpt/gpt-5.6-sol")
    );
    assert_eq!(
        settings.roles["precedence-senior"].effort,
        Some(tau_proto::Effort::Medium)
    );
    assert_eq!(
        settings.roles["precedence-senior"].verbosity,
        Some(tau_proto::Verbosity::Low)
    );
}

/// Ensures absent profile configuration loads only base settings instead of
/// assuming a profile named `default`.
#[test]
fn absent_default_profile_loads_only_base_configuration() {
    let td = TempDir::new().expect("tempdir");

    let settings = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect("base configuration should load");

    assert!(settings.roles.contains_key("engineer"));
}

/// Ensures user and drop-in patches to the configured fallback profile merge,
/// and that normal loading has the same result as explicit selection.
#[test]
fn default_profile_merges_user_and_drop_in_patches() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
extensions:
  local-tool:
    command: [tool]
    enable: true
default_profile: default
profiles:
  default:
    agents:
      default_role: default-role
      role_groups:
        default:
          roles:
            default-role: {}
"#,
    )
    .expect("write default profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-default.yaml"),
        r#"
profiles:
  default:
    extensions:
      local-tool:
        enable: false
"#,
    )
    .expect("write default profile drop-in");

    let dirs = dirs_with_config(td.path());
    let implicit = load_harness_settings_in(&dirs).expect("load configured default profile");
    let default = profile_name("default");
    let explicit =
        load_harness_settings_with_profile_and_cli_overrides_in(&dirs, Some(&default), &[], &[])
            .expect("load explicit default profile");

    assert_eq!(implicit.default_role.as_deref(), Some("default-role"));
    assert_eq!(implicit.extensions["local-tool"].enable, Some(false));
    assert_eq!(explicit.default_role, implicit.default_role);
    assert_eq!(
        explicit.extensions["local-tool"].enable,
        implicit.extensions["local-tool"].enable
    );
}

/// Ensures an explicit null in a later base layer clears an earlier fallback
/// selection, so callers load only base configuration.
#[test]
fn default_profile_null_clears_an_earlier_layer() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
default_profile: focused
profiles:
  focused:
    agents:
      default_role: engineer-senior
"#,
    )
    .expect("write fallback profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-clear.yaml"),
        "default_profile: null\n",
    )
    .expect("clear fallback profile");

    let dirs = dirs_with_config(td.path());
    assert_eq!(
        default_profile_in(&dirs).expect("read cleared fallback"),
        None
    );
    let settings = load_harness_settings_in(&dirs).expect("load base settings");
    assert_eq!(settings.default_role.as_deref(), Some("engineer"));
}

/// Ensures a configured fallback still validates its named profile rather than
/// silently falling back to base settings when its target is absent.
#[test]
fn default_profile_reports_an_unknown_target() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(td.path().join("harness.yaml"), "default_profile: missing\n")
        .expect("write missing fallback");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("unknown fallback profile");
    assert_eq!(
        error.to_string(),
        "unknown configuration profile: `missing`"
    );
}

/// Ensures an explicit named profile remains an independent base-layer patch
/// rather than inheriting the configured fallback profile.
#[test]
fn explicit_named_profile_does_not_inherit_default_profile() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
extensions:
  local-tool:
    command: [tool]
    enable: true
default_profile: default
profiles:
  default:
    extensions:
      local-tool:
        enable: false
  focused:
    agents:
      default_role: engineer-senior
"#,
    )
    .expect("write independent profiles");

    let dirs = dirs_with_config(td.path());
    let implicit = load_harness_settings_in(&dirs).expect("load implicit default profile");
    let focused = profile_name("focused");
    let explicit =
        load_harness_settings_with_profile_and_cli_overrides_in(&dirs, Some(&focused), &[], &[])
            .expect("load explicit named profile");

    assert_eq!(implicit.extensions["local-tool"].enable, Some(false));
    assert_eq!(explicit.extensions["local-tool"].enable, Some(true));
    assert_eq!(explicit.default_role.as_deref(), Some("engineer-senior"));
}

/// Ensures a role introduced by a selected profile inherits the group defaults
/// established by lower-precedence file layers.
#[test]
fn profile_role_inherits_group_defaults_from_file_layers() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    profile-inheritance:
      verbosity: high
profiles:
  focused:
    agents:
      role_groups:
        profile-inheritance:
          roles:
            profile-inherited: {}
"#,
    )
    .expect("write profile");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(
        settings.roles["profile-inherited"].verbosity,
        Some(tau_proto::Verbosity::High)
    );
}

/// Ensures profile and `--harness-config` group patches retain source order
/// while an earlier role-relative patch retains narrower-scope precedence.
#[test]
fn profile_and_cli_group_defaults_precede_role_overrides() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    precedence:
      roles:
        precedence-role:
          effort: decrease
profiles:
  focused:
    agents:
      role_groups:
        precedence:
          effort: high
"#,
    )
    .expect("write profile");
    let profile = profile_name("focused");
    let overrides =
        [
            HarnessConfigCliOverride::from_str("agents.role_groups.precedence.effort=increase")
                .expect("CLI group override"),
        ];

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &overrides,
    )
    .expect("load selected profile");
    assert_eq!(
        settings.roles["precedence-role"].effort,
        Some(tau_proto::Effort::High)
    );
}

/// Ensures a selected profile overlays file-layer role defaults before its
/// relative values resolve, while a later CLI layer still wins for extensions.
#[test]
fn selected_profile_merges_roles_before_cli_and_extensions() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  effort: low
  role_groups:
    base:
      roles:
        base-role: {}
profiles:
  focused:
    agents:
      effort: increase
      role_groups:
        profile:
          roles:
            profile-role:
              enable: false
              verbosity: high
    extensions:
      core-shell:
        enable: false
"#,
    )
    .expect("write base profile");
    let overrides = [
        HarnessConfigCliOverride::from_str("extensions.core-shell.enable=true")
            .expect("extension override"),
    ];

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[RoleCliOverride::Enable("profile-role".to_owned())],
        &overrides,
    )
    .expect("load selected profile");

    assert_eq!(
        settings.roles["base-role"].effort,
        Some(tau_proto::Effort::Medium)
    );
    assert_eq!(
        settings.roles["profile-role"].effort,
        Some(tau_proto::Effort::Medium)
    );
    assert_eq!(
        settings.roles["profile-role"].verbosity,
        Some(tau_proto::Verbosity::High)
    );
    assert_eq!(settings.extensions["core-shell"].enable, Some(true));
}

/// Ensures profile definitions merge through ordinary user/drop-in discovery,
/// then reject unknown selected names instead of silently using base settings.
#[test]
fn selected_profile_discovers_drop_ins_and_reports_unknown_names() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  focused:
    agents:
      role_groups:
        profile:
          roles:
            profile-role: {}
"#,
    )
    .expect("write profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-focused.yaml"),
        r#"
profiles:
  focused:
    extensions:
      core-shell:
        enable: false
"#,
    )
    .expect("write profile drop-in");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert!(settings.roles.contains_key("profile-role"));
    assert_eq!(settings.extensions["core-shell"].enable, Some(false));

    let profile = profile_name("missing");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect_err("unknown profile");
    assert_eq!(
        error.to_string(),
        "unknown configuration profile: `missing`"
    );
}

/// Ensures every selected profile source replays independently, preserving
/// relative provider adjustments rather than replacing an earlier profile map.
#[test]
fn selected_profile_replays_relative_settings_from_each_drop_in() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  effort: low
  role_groups:
    profile:
      roles:
        profile-role: {}
profiles:
  focused:
    agents:
      effort: increase
    extensions:
      core-shell:
        enable: false
"#,
    )
    .expect("write base profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-focused.yaml"),
        r#"
profiles:
  focused:
    agents:
      effort: increase
    extensions:
      core-shell:
        enable: true
"#,
    )
    .expect("write profile drop-in");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(
        settings.roles["profile-role"].effort,
        Some(tau_proto::Effort::High)
    );
    assert_eq!(settings.extensions["core-shell"].enable, Some(true));
}

/// Ensures a selected profile can choose a role that it adds after base
/// settings have loaded, so role construction completes before startup
/// selection observes the profile default.
#[test]
fn selected_profile_default_role_selects_profile_created_role() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  default_role: base-role
  role_groups:
    base:
      roles:
        base-role: {}
profiles:
  focused:
    agents:
      default_role: profile-role
      role_groups:
        profile:
          roles:
            profile-role: {}
"#,
    )
    .expect("write profile");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");

    assert_eq!(settings.default_role.as_deref(), Some("profile-role"));
    assert!(settings.roles.contains_key("profile-role"));
}

/// Ensures profile defaults replay in source order, accept the established
/// alias, and remain lower precedence than a later harness-config override.
#[test]
fn selected_profile_default_role_preserves_source_and_cli_precedence() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    roles:
      roles:
        base-role: {}
        first-profile-role: {}
        final-profile-role: {}
        cli-role: {}
profiles:
  focused:
    agents:
      defaultRole: first-profile-role
"#,
    )
    .expect("write base profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-focused.yaml"),
        r#"
profiles:
  focused:
    agents:
      default_role: final-profile-role
"#,
    )
    .expect("write profile drop-in");

    let profile = profile_name("focused");
    let base = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load profile sources");
    assert_eq!(base.default_role.as_deref(), Some("final-profile-role"));

    let overrides = [
        HarnessConfigCliOverride::from_str("agents.default_role=cli-role").expect("CLI override"),
    ];
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &overrides,
    )
    .expect("load profile with CLI override");
    assert_eq!(settings.default_role.as_deref(), Some("cli-role"));
}

/// Ensures an explicit null in a selected profile clears the base startup role,
/// matching the top-level nullable default-role configuration semantics.
#[test]
fn selected_profile_default_role_null_clears_base_default() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  default_role: base-role
profiles:
  focused:
    agents:
      default_role: null
"#,
    )
    .expect("write profile");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");

    assert_eq!(settings.default_role, None);
}

/// Ensures profiles accept only their explicit role and extension-enable
/// surface rather than becoming a recursive second harness configuration.
#[test]
fn selected_profile_rejects_unsupported_settings_and_extensions() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  invalid:
    session_retention_days: 1
"#,
    )
    .expect("write profiles");

    let profile = profile_name("invalid");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect_err("unsupported profile setting");
    assert!(error.to_string().contains("unknown field"), "{error}");

    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  invalid:
    extensions:
      core-shell:
        command: ["not-supported"]
"#,
    )
    .expect("write unsupported extension profile");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect_err("unsupported extension setting");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

/// Ensures one-shot harness config accepts relative provider defaults, starts
/// otherwise-unset values from neutral bases, and rejects a zero adjustment.
#[test]
fn harness_config_cli_relative_provider_defaults_use_neutral_bases() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                inherited: {}
        "#,
    )
    .expect("write harness");
    let overrides = [
        HarnessConfigCliOverride::from_str("agents.effort=increase").expect("effort"),
        HarnessConfigCliOverride::from_str("agents.verbosity=decrease:99").expect("verbosity"),
        HarnessConfigCliOverride::from_str("agents.thinking_summary=increase:99")
            .expect("thinking summary"),
    ];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");
    let inherited = &settings.roles["inherited"];
    assert_eq!(inherited.effort, Some(tau_proto::Effort::High));
    assert_eq!(inherited.verbosity, Some(tau_proto::Verbosity::Low));
    assert_eq!(
        inherited.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );

    let zero = [HarnessConfigCliOverride::from_str("agents.effort=increase:0").expect("parse")];
    assert!(
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &zero).is_err()
    );
}

/// Ensures command-line provider defaults, including legacy aliases, use the
/// same role replay path as defaults read from harness files.
#[test]
fn harness_config_cli_provider_defaults_apply_to_all_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            custom:
              roles:
                inherited: {}
        "#,
    )
    .expect("write harness");
    let overrides = [
        HarnessConfigCliOverride::from_str("agents.model=openai/cli-model").expect("model"),
        HarnessConfigCliOverride::from_str("agents.thinkingSummary=detailed")
            .expect("thinking summary"),
        HarnessConfigCliOverride::from_str("agents.serviceTier=fast").expect("service tier"),
    ];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");
    let inherited = &settings.roles["inherited"];
    assert_eq!(
        inherited.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/cli-model")
    );
    assert_eq!(
        inherited.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );
    assert_eq!(inherited.service_tier, Some(tau_proto::ServiceTier::Fast));
}

/// Ensures command-line agent-global prompt fragments are folded into every
/// role.
#[test]
fn harness_config_cli_global_prompt_fragments_apply_to_all_roles() {
    // One-shot harness config overrides are a convenient way to inject shared
    // run-specific instructions. They must take the same domain-specific merge
    // path as file-based `agents.prompt_fragments` so every effective role sees
    // the fragment exactly once.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                custom: {
                    roles: {
                        custom: { model: "openai/custom" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write harness");

    let s = load_harness_settings_with_cli_overrides_in(
        &dirs_with_config(dir),
        &[],
        &[HarnessConfigCliOverride::from_str(
            "agents.promptFragments=[{ name: \"global.cli\", priority: 64, text: \"Follow the run policy.\" }]",
        )
        .expect("parse override")],
    )
    .expect("load");

    assert_eq!(
        s.prompt_fragments
            .iter()
            .filter(|fragment| fragment.name == "global.cli")
            .count(),
        1
    );
    assert!(s.roles.contains_key("custom"));
    for (role_name, role) in &s.roles {
        assert_eq!(
            role.prompt_fragments
                .iter()
                .filter(|fragment| fragment.name == "global.cli")
                .count(),
            1,
            "CLI global fragment should apply once to {role_name}"
        );
    }
}

/// Ensures user role definitions merge with the built-in role catalog rather
/// than replacing it wholesale.
#[test]
fn harness_roles_merge_with_built_ins() {
    // Roles are harness-owned now. This keeps the old merge behavior while
    // locking the source of truth to harness.yaml instead of a model registry.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    roles: {
                        engineer: { model: "openai/gpt-5.5", tools: ["read"] },
                        custom: { description: "Custom local role", effort: "medium", disable_tools: ["shell"] },
                    },
                },
                manager: {
                    roles: {
                        "project-manager": { model: "openai/gpt-5.5" },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("engineer"));
    assert!(s.roles.contains_key("project-manager"));
    assert!(!s.roles.contains_key("assistant"));
    assert!(!s.roles.contains_key("smart"));
    assert!(!s.roles.contains_key("deep"));
    assert!(!s.roles.contains_key("rush"));
    assert!(!s.roles.contains_key("foreman"));
    assert!(!s.roles.contains_key("default"));
    assert_eq!(
        s.roles["custom"].description.as_deref(),
        Some("Custom local role")
    );
    assert_eq!(s.roles["custom"].effort, Some(tau_proto::Effort::Medium));
    assert_eq!(
        s.roles["custom"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
    );
    assert_eq!(
        s.roles["engineer"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("openai/gpt-5.5")
    );
    assert_eq!(
        s.roles["engineer"].tools,
        Some(vec![tau_proto::ToolName::new("read")])
    );

    assert_eq!(
        s.roles["project-manager"]
            .model
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("openai/gpt-5.5")
    );
}

/// Ensures role group fields act as defaults for roles in that group.
#[test]
fn harness_role_group_fields_apply_as_role_defaults() {
    // Group-level role fields keep shared role policy in one place. Individual
    // roles can still override scalar defaults or add their own fragments.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                review: {
                    effort: "low",
                    tools: ["read"],
                    enable_tools: ["grep"],
                    prompt_fragments: [
                        { name: "review.shared", priority: 80, text: "Review carefully." },
                    ],
                    roles: {
                        quick: {},
                        deep: {
                            effort: "xhigh",
                            prompt_fragments: [
                                { name: "review.deep", priority: 90, text: "Look for subtle issues." },
                            ],
                        },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let quick = &s.roles["quick"];
    assert_eq!(quick.effort, Some(tau_proto::Effort::Low));
    assert_eq!(quick.tools, Some(vec![tau_proto::ToolName::new("read")]));
    assert_eq!(quick.enable_tools, vec![tau_proto::ToolName::new("grep")]);
    assert!(
        quick
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.shared")
    );

    let deep = &s.roles["deep"];
    assert_eq!(deep.effort, Some(tau_proto::Effort::XHigh));
    assert!(
        deep.prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.shared")
    );
    assert!(
        deep.prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.deep")
    );
}

/// Ensures role prompt fragments may be specified as plain string entries.
#[test]
fn harness_role_prompt_fragments_parse_as_plain_strings() {
    // Role prompt customization must keep harness.yaml ergonomic: users write
    // prompt text directly instead of nested newtype objects.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                review: {
                    roles: {
                        custom: {
                            prompt_fragments: [
                                { name: "custom.reviewer", priority: 100, text: "You are a focused reviewer." },
                                { name: "custom.patch-style", priority: 200, text: "Prefer small patches." },
                            ],
                        },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let role = &s.roles["custom"];
    assert_eq!(
        role.prompt_fragments
            .first()
            .map(|fragment| fragment.text.as_str()),
        Some("You are a focused reviewer.")
    );
    assert_eq!(
        role.prompt_fragments
            .get(1)
            .map(|fragment| fragment.text.as_str()),
        Some("Prefer small patches.")
    );
}

/// Ensures the embedded built-in role catalog contains only engineer roles and
/// gives each role the capability-gated delegate-role fragment.
#[test]
fn harness_built_in_roles_load_with_global_delegate_role_prompt() {
    // The available-role list is shared across roles, but its Handlebars guard
    // leaves prompts unchanged when `agent_start` is absent.
    let s = HarnessSettings::built_in();
    assert_eq!(s.default_role.as_deref(), Some("engineer"));
    assert_eq!(
        s.role_groups
            .iter()
            .map(|group| (group.name.clone(), group.roles.clone()))
            .collect::<Vec<_>>(),
        vec![(
            "engineer".to_owned(),
            vec![
                "engineer-junior".to_owned(),
                "engineer".to_owned(),
                "engineer-senior".to_owned(),
            ],
        )]
    );
    let mut role_names = s.roles.keys().map(String::as_str).collect::<Vec<_>>();
    role_names.sort_unstable();
    assert_eq!(
        role_names,
        vec!["engineer", "engineer-junior", "engineer-senior"]
    );
    let engineer_junior = &s.roles["engineer-junior"];
    assert_eq!(engineer_junior.effort, Some(tau_proto::Effort::Low));
    let engineer = &s.roles["engineer"];
    let delegate_roles = engineer
        .prompt_fragments
        .iter()
        .find(|fragment| fragment.name == "agent.available-roles")
        .expect("global delegate-role prompt fragment");
    assert_eq!(delegate_roles.priority, PromptPriority::new(6));
    assert!(
        delegate_roles
            .text
            .contains("{{#if (tool_available capabilities.tools \"agent_start\")~}}")
    );
    assert!(delegate_roles.text.contains("## Available sub-task roles"));
    let engineer_instructions = engineer
        .prompt_fragments
        .iter()
        .find(|fragment| fragment.name == "engineer.instructions")
        .expect("engineer prompt fragment");
    assert_eq!(engineer_instructions.priority, PromptPriority::new(15));
    assert_eq!(
        engineer_instructions
            .text
            .lines()
            .filter(|line| *line == "## Best practices")
            .count(),
        1
    );
    assert_eq!(
        engineer_instructions
            .text
            .lines()
            .filter(|line| *line == "### Best practices")
            .count(),
        0
    );
    assert!(!s.roles.contains_key("assistant"));
    let engineer_senior = &s.roles["engineer-senior"];
    assert_eq!(engineer_senior.effort, Some(tau_proto::Effort::High));
}

/// Ensures user-defined role groups can load custom role definitions.
#[test]
fn harness_role_groups_load_custom_roles() {
    // Role groups are the user-facing role configuration shape.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                coding: {
                    roles: {
                        custom: { effort: "medium", tools: ["read"] },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.roles["custom"].effort, Some(tau_proto::Effort::Medium));
    assert_eq!(
        s.roles["custom"].tools.as_ref().expect("tools"),
        &vec![tau_proto::ToolName::new("read")]
    );
}

/// Ensures role `order` values load as ordinary role fields so the harness can
/// sort keyboard navigation within each role group independently from role
/// name.
#[test]
fn harness_role_groups_load_role_order() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                engineer: {
                    order: 40,
                    roles: {
                        "engineer-senior": { order: null },
                        "engineer": { order: 20 },
                        "engineer-junior": {},
                        "custom-engineer": {},
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(s.roles["engineer-junior"].order, Some(10));
    assert_eq!(s.roles["engineer"].order, Some(20));
    assert_eq!(s.roles["engineer-senior"].order, None);
    assert_eq!(s.roles["custom-engineer"].order, Some(40));
}

/// Ensures harness custom prompts parse from map syntax, sort by id, and are
/// available by stable id for the CLI `:prompt <id>` command.
#[test]
fn harness_custom_prompts_parse_from_config() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"custom_prompts:
  summarize: |
    Summarize the current session.
  review: "Review this code carefully"
"#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(
        settings.custom_prompts,
        vec![
            CustomPrompt {
                id: "review".to_owned(),
                text: "Review this code carefully".to_owned(),
            },
            CustomPrompt {
                id: "summarize".to_owned(),
                text: "Summarize the current session.\n".to_owned(),
            },
        ]
    );
}

/// Ensures invalid custom prompt ids fail during config loading instead of
/// producing ambiguous or unreachable `:prompt <id>` commands.
#[test]
fn harness_custom_prompts_reject_empty_and_whitespace_ids() {
    for (yaml, expected) in [
        ("custom_prompts:\n  '': hello\n", "must not be empty"),
        (
            "custom_prompts:\n  'bad id': hello\n",
            "must not contain whitespace",
        ),
    ] {
        let td = TempDir::new().expect("tempdir");
        let dir = td.path();
        std::fs::write(dir.join("harness.yaml"), yaml).expect("write");

        let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject prompt");

        assert!(
            error.to_string().contains(expected),
            "error should contain `{expected}`: {error}"
        );
    }
}

/// Ensures empty custom prompt text is rejected because selecting it would look
/// like a successful no-op rather than a reusable prompt template.
#[test]
fn harness_custom_prompts_reject_empty_text() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("harness.yaml"), "custom_prompts:\n  empty: ''\n").expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("reject empty text");

    assert!(
        error.to_string().contains("text must not be empty"),
        "error should explain empty text: {error}"
    );
}

/// Ensures duplicate role names across role groups are rejected explicitly.
#[test]
fn harness_role_groups_reject_duplicate_role_names() {
    // Role names are runtime identities, so grouping is only navigation; the
    // same role name in two groups would make keyboard traversal ambiguous.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                coding: { roles: { engineer: {} } },
                review: { roles: { engineer: {} } },
            },
            },
        }"#,
    )
    .expect("write");

    let err = load_harness_settings_in(&dirs_with_config(dir)).expect_err("duplicate role");
    assert!(err.to_string().contains("appears in multiple role_groups"));
}

/// Inter-session capabilities inherit as ordinary role fields and retain the
/// scalar absent/null/value layering contract.
#[test]
fn inter_session_capabilities_inherit_and_override_per_role() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    manager:
      inter_session_receiver: true
      inter_session_auto_start: true
      roles:
        project-manager: {}
        task-manager:
          inter_session_auto_start: false
"#,
    )
    .expect("write base");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load receiver policy");
    assert_eq!(
        settings.roles["task-manager"].inter_session_receiver,
        Some(true)
    );
    assert_eq!(
        settings.roles["task-manager"].inter_session_auto_start,
        Some(false)
    );
    assert_eq!(
        settings.roles["project-manager"].inter_session_auto_start,
        Some(true)
    );

    std::fs::create_dir(td.path().join("harness.d")).expect("drop-in dir");
    std::fs::write(
        td.path().join("harness.d/10-clear.yaml"),
        "agents: { role_groups: { manager: { inter_session_receiver: null, inter_session_auto_start: null } } }",
    )
    .expect("write clear");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("clear inherited fields");
    assert_eq!(
        settings.roles["project-manager"].inter_session_receiver,
        None
    );
    assert_eq!(
        settings.roles["project-manager"].inter_session_auto_start,
        None
    );
    assert_eq!(settings.roles["task-manager"].inter_session_receiver, None);
    assert_eq!(
        settings.roles["task-manager"].inter_session_auto_start,
        Some(false),
        "role override remains effective over the later group clear"
    );
}

/// Receiver and auto-start capabilities may be enabled across multiple groups.
#[test]
fn inter_session_capabilities_allow_multiple_groups() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    engineer:
      inter_session_receiver: true
      inter_session_auto_start: true
    manager:
      inter_session_receiver: true
      inter_session_auto_start: true
      roles:
        project-manager: {}
"#,
    )
    .expect("write");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("multiple receiver groups");
    assert!(
        settings.roles["engineer"]
            .inter_session_auto_start
            .unwrap_or(false)
    );
    assert!(
        settings.roles["project-manager"]
            .inter_session_auto_start
            .unwrap_or(false)
    );
}

/// Auto-start spending authority without receiver authority is rejected after
/// role inheritance, while disabled incoherent roles are irrelevant.
#[test]
fn inter_session_auto_start_requires_receiver_on_enabled_roles() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    manager:
      roles:
        project-manager:
          inter_session_auto_start: true
"#,
    )
    .expect("write");
    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("incoherent role");
    assert!(error.to_string().contains(
        "role `project-manager` enables `inter_session_auto_start` without `inter_session_receiver`"
    ));

    std::fs::write(
        td.path().join("harness.yaml"),
        "agents: { role_groups: { manager: { roles: { project-manager: { enable: false, inter_session_auto_start: true } } } } }",
    )
    .expect("disable incoherent role");
    load_harness_settings_in(&dirs_with_config(td.path())).expect("disabled role is removed first");
}

/// Removed peer-entrypoint keys fail explicitly instead of being silently
/// ignored or mixed with role capabilities.
#[test]
fn inter_session_configuration_rejects_removed_peer_entrypoint_schema() {
    for yaml in [
        "agents: { role_groups: { manager: { peer_entrypoint: {} } } }",
        "agents: { role_groups: { manager: { peerEntryPoint: { autoStartRole: project-manager } } } }",
    ] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(td.path().join("harness.yaml"), yaml).expect("write removed schema");

        let error =
            load_harness_settings_in(&dirs_with_config(td.path())).expect_err("reject old schema");

        assert!(
            error.to_string().contains("unknown field"),
            "unexpected error: {error}"
        );
    }
}

/// Canonical and camel-case spellings cannot both set the same receiver
/// capability in one source layer.
#[test]
fn inter_session_configuration_rejects_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    manager:
      interSessionReceiver: true
      inter_session_receiver: false
"#,
    )
    .expect("write conflict");

    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("reject alias conflict");

    assert!(
        error.to_string().contains("interSessionReceiver")
            && error.to_string().contains("inter_session_receiver"),
        "unexpected error: {error}"
    );
}

/// Dotted CLI overrides use the same alias normalization, group inheritance,
/// and per-role override behavior as file layers.
#[test]
fn inter_session_configuration_layers_cli_aliases() {
    let td = TempDir::new().expect("tempdir");
    let overrides = [
        HarnessConfigCliOverride::from_str("agents.roleGroups.engineer.interSessionReceiver=true")
            .expect("receiver override"),
        HarnessConfigCliOverride::from_str("agents.roleGroups.engineer.interSessionAutoStart=true")
            .expect("auto-start override"),
        HarnessConfigCliOverride::from_str(
            "agents.roleGroups.engineer.roles.engineer-senior.interSessionAutoStart=false",
        )
        .expect("role override"),
    ];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(td.path()), &[], &overrides)
            .expect("load aliases");

    assert_eq!(
        settings.roles["engineer-senior"].inter_session_receiver,
        Some(true)
    );
    assert_eq!(
        settings.roles["engineer-senior"].inter_session_auto_start,
        Some(false)
    );
}

/// Ensures absent user config files still load the built-in harness baseline.
#[test]
fn missing_user_files_load_the_built_in_baseline() {
    // With no user files present, the loader still returns fully populated
    // settings from the embedded built-in layer plus harness-owned role defaults.
    // There is intentionally no model registry baseline anymore.
    let td = TempDir::new().expect("tempdir");
    let _cli = load_cli_settings_in(&dirs_with_config(td.path())).expect("cli");
    let harness = load_harness_settings_in(&dirs_with_config(td.path())).expect("harness");
    assert!(harness.roles.contains_key("engineer-junior"));
    assert_eq!(harness.tau_state_access, TauStateAccess::Hidden);
    assert!(harness.roles.contains_key("engineer"));
    assert_eq!(harness.default_role.as_deref(), Some("engineer"));
    assert_eq!(harness.roles["engineer-junior"].enable, Some(true));
    assert_eq!(harness.roles["engineer"].enable, Some(true));
    assert!(!harness.roles.contains_key("assistant"));
    assert!(harness.roles.contains_key("engineer-senior"));
    assert_eq!(harness.roles["engineer-senior"].enable, Some(true));
    assert_eq!(
        harness.roles["engineer-senior"].effort,
        Some(tau_proto::Effort::High)
    );
    assert!(!harness.roles.contains_key("smart"));
    assert!(!harness.roles.contains_key("deep"));
    assert!(!harness.roles.contains_key("rush"));
    assert!(!harness.roles.contains_key("foreman"));
}

/// Ensures `enable: false` removes lower-layer roles only after all role layers
/// merge.
#[test]
fn harness_role_enable_false_filters_built_in_roles_after_merging() {
    // `enable: false` is the merge-friendly way to remove a role supplied by a
    // lower layer: the role can keep its inherited config shape, but disappears
    // from the effective role map and navigation groups after all layers merge.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                default_role: "engineer",
                role_groups: {
                engineer: {
                    roles: {
                        "engineer-junior": { enable: false },
                        "engineer": { enable: false },
                        "engineer-senior": { enable: false },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.roles.contains_key("engineer-junior"));
    assert!(!s.roles.contains_key("engineer"));
    assert!(!s.roles.contains_key("engineer-senior"));
    assert!(!s.roles.contains_key("assistant"));
    assert_eq!(s.default_role.as_deref(), Some("engineer"));
    assert!(s.role_groups.is_empty());
}

/// Ensures agent, group, and role enablement use their ordinary scope
/// precedence, with each narrower scope overriding the broader one.
#[test]
fn harness_agent_enable_precedes_group_and_role_enablement() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  enable: false
  role_groups:
    precedence:
      enable: true
      roles:
        group-enabled: {}
        role-disabled:
          enable: false
"#,
    )
    .expect("write config");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    assert_eq!(settings.roles["group-enabled"].enable, Some(true));
    assert!(!settings.roles.contains_key("role-disabled"));
}

/// Ensures an explicit higher-layer `null` clears an earlier global disable and
/// restores the ordinary enabled role behavior without adding a reset barrier.
#[test]
fn harness_agent_enable_null_clears_an_earlier_disable() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  enable: false
"#,
    )
    .expect("write base config");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/10-clear-enable.yaml"),
        r#"
agents:
  enable: null
"#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    assert!(settings.roles.contains_key("engineer"));
    assert_eq!(settings.roles["engineer"].enable, None);
}

/// Ensures a selected profile can disable the normal role catalog globally and
/// explicitly retain only roles it re-enables at the narrow role scope.
#[test]
fn profile_can_disable_all_roles_and_selectively_reenable_roles() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  focused:
    agents:
      enable: false
      role_groups:
        engineer:
          roles:
            engineer:
              enable: true
"#,
    )
    .expect("write profile");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(settings.roles.len(), 1);
    assert_eq!(settings.roles["engineer"].enable, Some(true));
}

/// Ensures explicit base group and role enables remain narrower than a selected
/// profile's later agent-wide disablement.
#[test]
fn base_group_and_role_enablement_override_profile_agent_disable() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  enable: false
  role_groups:
    group-pinned:
      enable: true
      roles:
        group-role: {}
    role-pinned:
      roles:
        role-role:
          enable: true
profiles:
  focused:
    agents:
      enable: false
"#,
    )
    .expect("write config");

    let profile = profile_name("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(settings.roles.len(), 2);
    assert_eq!(settings.roles["group-role"].enable, Some(true));
    assert_eq!(settings.roles["role-role"].enable, Some(true));
}

/// Ensures normal top-level `--harness-config` paths participate in agent
/// enablement and can pair a broad disable with a role-level re-enable.
#[test]
fn harness_config_cli_can_disable_agents_and_reenable_one_role() {
    let td = TempDir::new().expect("tempdir");
    let overrides = [
        HarnessConfigCliOverride::from_str("agents.enable=false").expect("disable agents"),
        HarnessConfigCliOverride::from_str(
            "agents.role_groups.engineer.roles.engineer.enable=true",
        )
        .expect("enable engineer"),
    ];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(td.path()), &[], &overrides)
            .expect("load CLI overrides");
    assert_eq!(settings.roles.len(), 1);
    assert_eq!(settings.roles["engineer"].enable, Some(true));
}

/// Ensures legacy `enabled` role fields continue to disable roles in old config
/// files.
#[test]
fn harness_role_enabled_alias_is_kept_for_old_config() {
    // `enabled` was a mistaken old spelling. Keep accepting it as a little
    // bandaid so existing configs keep loading while users migrate to `enable`.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            agents: {
                role_groups: {
                legacy: {
                    enabled: false,
                    roles: {
                        old_on: { enabled: true },
                        old_off: {},
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(s.roles["old_on"].enable, Some(true));
    assert!(!s.roles.contains_key("old_off"));
    assert_eq!(
        s.role_groups
            .iter()
            .find(|group| group.name == "legacy")
            .map(|group| group.roles.as_slice()),
        Some(&["old_on".to_owned()][..])
    );
}

/// Regression guard: legacy `enabled` disables built-in roles after alias
/// normalization.
#[test]
fn harness_legacy_enabled_alias_overrides_built_in_enable() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
        agents:
          role_groups:
            engineer:
              roles:
                engineer:
                  enabled: false
        "#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert!(!settings.roles.contains_key("engineer"));
}

/// Regression guard: role filtering happens after all layers so later enables
/// win.
#[test]
fn harness_role_enable_can_be_reenabled_by_later_layers() {
    // Filtering happens after the complete domain merge, so a higher-priority
    // drop-in can re-enable a role disabled by the base user config.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir drop-ins");
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{ agents: { role_groups: { engineer: { roles: { "engineer-senior": { enable: false } } } } } }"#,
    )
    .expect("write base");
    std::fs::write(
        dir.join("harness.d/10-enable.yaml"),
        r#"{ agents: { role_groups: { engineer: { roles: { "engineer-senior": { enable: true, effort: "xhigh" } } } } } }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("engineer-senior"));
    assert_eq!(s.roles["engineer-senior"].enable, Some(true));
    assert!(s.role_groups.iter().any(|group| group.name == "engineer"
        && group.roles.iter().any(|role| role == "engineer-senior")));
}

/// Ensures sample config files shipped for `tau init` keep deserializing.
#[test]
fn sample_configs_deserialize() {
    // Sanity-check the sample configs shipped in the workspace root `config/`
    // directory (used by `tau init`) by feeding them through the user-config
    // loader.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();

    std::fs::write(
        dir.join("cli.yaml"),
        include_str!("../../../../config/cli.yaml"),
    )
    .expect("write cli");
    std::fs::write(
        dir.join("harness.yaml"),
        include_str!("../../../../config/harness.yaml"),
    )
    .expect("write harness");

    let _cli = load_cli_settings_in(&dirs_with_config(dir)).expect("cli sample should parse");
    let _harness =
        load_harness_settings_in(&dirs_with_config(dir)).expect("harness sample should parse");
}

/// Documents accepted/rejected extension names for path and CLI override
/// safety.
#[test]
fn extension_state_dir_rejects_unsafe_extension_names() {
    // Extension names can come from user-authored harness.yaml keys. Rejecting
    // anything outside the conservative extension-name character set keeps the
    // injected state directory confined under state/ext/<extension> and avoids
    // ambiguity in dotted harness config override paths.
    let state_dir = path_std_path::Path::new("/tmp/tau-state");
    for name in ["a", "a_b", "x9", "std-email"] {
        assert_eq!(
            extension_state_dir_of(state_dir, name).expect("safe extension name"),
            state_dir.join("ext").join(name)
        );
    }
    let longest = "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES);
    assert!(extension_state_dir_of(state_dir, &longest).is_ok());

    for name in ["", "../x", "a/b", "/tmp/x", ".", "..", "foo.bar"] {
        assert!(
            extension_state_dir_of(state_dir, name).is_err(),
            "{name:?} must be rejected"
        );
    }
    let oversized = "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES + 1);
    assert!(extension_state_dir_of(state_dir, &oversized).is_err());
}

/// Harness config loading accepts the exact extension-name byte limit and
/// rejects the next byte before the name can reach path construction.
#[test]
fn harness_settings_enforce_extension_name_length_boundary() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let accepted = "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES);
    std::fs::write(
        dir.join("harness.yaml"),
        format!("extensions:\n  {accepted}:\n    command: [/bin/true]\n"),
    )
    .expect("write accepted config");
    let loaded =
        load_harness_settings_in(&dirs_with_config(dir)).expect("128-byte name should load");
    assert!(loaded.extensions.contains_key(&accepted));

    let rejected = "x".repeat(tau_proto::EXTENSION_NAME_MAX_BYTES + 1);
    std::fs::write(
        dir.join("harness.yaml"),
        format!("extensions:\n  {rejected}:\n    command: [/bin/true]\n"),
    )
    .expect("write rejected config");
    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("129-byte name must fail");
    assert!(error.to_string().contains("at most 128 ASCII bytes"));
}

/// Regression guard: invalid extension keys in harness.yaml fail at load time.
#[test]
fn harness_settings_reject_invalid_extension_names() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  ../evil:
    command: [evil]
"#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("invalid extension");

    assert!(
        error.to_string().contains("../evil"),
        "unexpected error: {error}"
    );
}

/// Regression guard: CLI-created extension entries also validate names at load
/// time.
#[test]
fn harness_config_cli_overrides_reject_invalid_extension_names() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides =
        [
            HarnessConfigCliOverride::from_str(r#"extensions={"../evil": {command: [evil]}}"#)
                .expect("override"),
        ];

    let error =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect_err("invalid extension");

    assert!(
        error.to_string().contains("../evil"),
        "unexpected error: {error}"
    );
}

/// Regression guard: drop-in `cwd: null` clears an inherited extension cwd.
#[test]
fn harness_extension_drop_in_can_clear_inherited_cwd() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  local-tool:
    command: [tool]
    cwd: /tmp/lower
"#,
    )
    .expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d/10-clear.yaml"),
        r#"
extensions:
  local-tool:
    cwd: null
"#,
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(settings.extensions["local-tool"].cwd, Some(None));
}

/// Regression guard: CLI `cwd=null` clears an inherited extension cwd.
#[test]
fn harness_config_cli_overrides_can_clear_extension_cwd() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  local-tool:
    command: [tool]
    cwd: /tmp/lower
"#,
    )
    .expect("write base");
    let overrides =
        [HarnessConfigCliOverride::from_str("extensions.local-tool.cwd=null").expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert_eq!(settings.extensions["local-tool"].cwd, Some(None));
}

/// Ensures extension secret declarations default to required secrets.
#[test]
fn harness_extension_secrets_parse_with_required_default() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  std-email:
    secrets:
      mail_password: {}
      optional_token:
        optional: true
"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let secrets = s.extensions["std-email"].secrets.as_ref().expect("secrets");
    assert!(!secrets["mail_password"].optional);
    assert!(secrets["optional_token"].optional);
}

/// Ensures extension secret entries reject unknown fields so typos are not
/// ignored.
#[test]
fn harness_extension_secret_entries_deny_unknown_fields() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
extensions:
  std-email:
    secrets:
      mail_password:
        bogus: true
"#,
    )
    .expect("write");

    let err = load_harness_settings_in(&dirs_with_config(dir)).expect_err("unknown field rejected");
    assert!(err.to_string().contains("bogus"), "unexpected error: {err}");
}

/// Per-extension tool prefixes accept the normalized camel-case spelling and
/// explicit null clears a lower-precedence value.
#[test]
fn harness_extension_tool_prefix_layers_and_clears() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        "extensions:\n  work:\n    command: [demo]\n    toolPrefix: team_ops\n",
    )
    .expect("write base");
    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load base");
    assert_eq!(
        settings.extensions["work"]
            .tool_prefix
            .as_ref()
            .and_then(Option::as_ref)
            .map(tau_proto::ToolNamePrefix::as_str),
        Some("team_ops")
    );
    std::fs::create_dir(dir.join("harness.d")).expect("drop-in dir");
    std::fs::write(
        dir.join("harness.d/10-clear.yaml"),
        "extensions:\n  work:\n    tool_prefix: null\n",
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(settings.extensions["work"].tool_prefix, Some(None));
}

/// Invalid segmented prefix syntax is rejected at the configuration boundary.
#[test]
fn harness_extension_tool_prefix_rejects_hyphens() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "extensions:\n  work:\n    command: [demo]\n    tool_prefix: team-ops\n",
    )
    .expect("write");
    let error = load_harness_settings_in(&dirs_with_config(td.path())).expect_err("invalid prefix");
    assert!(error.to_string().contains("invalid tool prefix"));
}

/// Ensures global access defaults layer below an instance-specific override.
#[test]
fn tau_state_access_supports_global_and_instance_configuration() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "tau_state_access: hidden\nextensions:\n  shell:\n    command: [demo]\n    tau_state_access: read_only\n",
    )
    .expect("write");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    assert_eq!(settings.tau_state_access, TauStateAccess::Hidden);
    assert_eq!(
        settings.extensions["shell"].tau_state_access,
        Some(TauStateAccess::ReadOnly)
    );
}
