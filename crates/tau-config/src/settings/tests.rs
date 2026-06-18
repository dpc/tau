use std::str::FromStr;

use tempfile::TempDir;

use super::*;

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
    }
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
    let output = std::process::Command::new("mkfifo")
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

/// Ensures `testing.yaml` parses exact provider profile names through the same
/// provider-name validator used by model routing and provider auth filenames.
#[test]
fn testing_settings_parses_testing_provider_allowlist() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        td.path().join("testing.yaml"),
        "testing_providers:\n  - chatgpt\n  - openrouter.work\n",
    )
    .expect("write testing settings");

    let loaded = load_testing_settings(&dirs_with_config(td.path()))
        .expect("load testing")
        .expect("present testing settings");

    assert_eq!(
        loaded.testing_providers,
        vec![
            tau_proto::ProviderName::new("chatgpt"),
            tau_proto::ProviderName::new("openrouter.work")
        ]
    );
}

/// Prevents malformed or path-like provider names in `testing.yaml` from being
/// accepted, because the allowlist is later translated into auth.d filenames.
#[test]
fn testing_settings_rejects_unsafe_provider_names() {
    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        td.path().join("testing.yaml"),
        "testing_providers:\n  - ../chatgpt\n",
    )
    .expect("write testing settings");

    let error = load_testing_settings(&dirs_with_config(td.path()))
        .expect_err("unsafe provider name rejected");

    assert!(error.to_string().contains("provider name"));
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
    assert_eq!(rule.enable_tool_tags.len(), 4);
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
    assert_eq!(rule.enable_tool_tags.len(), 4);
}

/// Ensures user CLI scalar settings override the built-in defaults.
#[test]
fn cli_settings_user_scalar_override_wins_over_built_in() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("cli.yaml"),
        r#"{ greeting: false, show_thinking: false, show_tools: "compact", show_messages: "self-summary", show_status: "minimal" }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_thinking);
    assert_eq!(s.show_tools, ShowTools::Compact);
    assert_eq!(s.show_messages, ShowMessages::SelfSummary);
    assert_eq!(s.show_status, ShowStatus::Minimal);
    assert_eq!(s.theme, CliTheme::Named("tau-plain-dark".to_owned()));
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
    assert_eq!(s.bind.get("C-p").expect("C-p").action, "prompt-previous");
    assert_eq!(s.bind.get("C-n").expect("C-n").action, "prompt-next");
}
/// Ensures user completion additions preserve built-in slash-command prefixes
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
        show_ui_io: true,
        show_tools: crate::settings::ShowTools::SummarizeTurn,
        show_messages: crate::settings::ShowMessages::AllSummary,
        notice_level: tau_proto::NoticeLevel::Debug,
        show_status: crate::settings::ShowStatus::Minimal,
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
    std::fs::write(dir.join("harness.yaml"), "defaultRole: legacy\n").expect("write base");
    std::fs::create_dir_all(dir.join("harness.d")).expect("mkdir dropins");
    std::fs::write(
        dir.join("harness.d").join("10-role.yaml"),
        "default_role: canonical\n",
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
    std::fs::write(dir.join("harness.yaml"), "defaultRole: legacy\n").expect("write base");
    let override_ = HarnessConfigCliOverride::from_str("default_role=cli").expect("override");

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &[override_])
            .expect("load");

    assert_eq!(settings.default_role.as_deref(), Some("cli"));
}

/// Ensures a single source cannot specify both top-level legacy and canonical
/// keys.
#[test]
fn harness_rejects_same_layer_top_level_alias_conflict() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        "defaultRole: legacy\ndefault_role: canonical\n",
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
        "defaultRole": "manager",
        "promptFragments": [],
        "customPrompts": [],
        "toolPolicy": {
            "rules": {
                "builtin.chatgpt-shell": {
                    "enabled": false,
                }
            }
        },
        "agents": {
            "idTemplate": "agent-{{random_alphanumeric 4}}",
            "displayNameTemplate": "Agent {{n}}",
        },
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
                "roles": {
                    "senior-engineer": {
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
                    }
                }
            }
        }
    });

    normalize_harness_config_value(&mut value, "test").expect("normalize");

    assert!(value.get("default_role").is_some());
    assert!(value.get("prompt_fragments").is_some());
    assert!(value.get("custom_prompts").is_some());
    assert!(value.get("tool_policy").is_some());
    assert!(
        value
            .pointer("/tool_policy/rules/builtin.chatgpt-shell/enable")
            .is_some()
    );
    assert!(value.pointer("/agents/id_template").is_some());
    assert!(value.pointer("/agents/display_name_template").is_some());
    let group = value.pointer("/role_groups/engineer").expect("group");
    for key in [
        "enable",
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
    ] {
        assert!(group.get(key).is_some(), "missing group key {key}");
        assert!(
            group
                .pointer(&format!("/roles/senior-engineer/{key}"))
                .is_some(),
            "missing role key {key}"
        );
    }
}

/// Ensures every maintained CLI override legacy alias normalizes to its
/// canonical path.
#[test]
fn harness_cli_alias_table_normalizes_all_legacy_keys() {
    let cases = [
        ("defaultRole", "default_role"),
        ("promptFragments", "prompt_fragments"),
        ("customPrompts", "custom_prompts"),
        ("toolPolicy", "tool_policy"),
        ("agents.idTemplate", "agents.id_template"),
        ("agents.displayNameTemplate", "agents.display_name_template"),
        ("roleGroups.engineer.enabled", "role_groups.engineer.enable"),
        (
            "roleGroups.engineer.thinkingSummary",
            "role_groups.engineer.thinking_summary",
        ),
        (
            "roleGroups.engineer.serviceTier",
            "role_groups.engineer.service_tier",
        ),
        (
            "roleGroups.engineer.promptFragments",
            "role_groups.engineer.prompt_fragments",
        ),
        (
            "roleGroups.engineer.promptOverride",
            "role_groups.engineer.prompt_override",
        ),
        (
            "roleGroups.engineer.disableToolTags",
            "role_groups.engineer.disable_tool_tags",
        ),
        (
            "roleGroups.engineer.enableToolTags",
            "role_groups.engineer.enable_tool_tags",
        ),
        (
            "roleGroups.engineer.enableToolGroups",
            "role_groups.engineer.enable_tool_groups",
        ),
        (
            "roleGroups.engineer.disableToolGroups",
            "role_groups.engineer.disable_tool_groups",
        ),
        (
            "roleGroups.engineer.enableTools",
            "role_groups.engineer.enable_tools",
        ),
        (
            "roleGroups.engineer.disableTools",
            "role_groups.engineer.disable_tools",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.enabled",
            "role_groups.engineer.roles.senior-engineer.enable",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.thinkingSummary",
            "role_groups.engineer.roles.senior-engineer.thinking_summary",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.serviceTier",
            "role_groups.engineer.roles.senior-engineer.service_tier",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.promptFragments",
            "role_groups.engineer.roles.senior-engineer.prompt_fragments",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.promptOverride",
            "role_groups.engineer.roles.senior-engineer.prompt_override",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.disableToolTags",
            "role_groups.engineer.roles.senior-engineer.disable_tool_tags",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.enableToolTags",
            "role_groups.engineer.roles.senior-engineer.enable_tool_tags",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.enableToolGroups",
            "role_groups.engineer.roles.senior-engineer.enable_tool_groups",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.disableToolGroups",
            "role_groups.engineer.roles.senior-engineer.disable_tool_groups",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.enableTools",
            "role_groups.engineer.roles.senior-engineer.enable_tools",
        ),
        (
            "roleGroups.engineer.roles.senior-engineer.disableTools",
            "role_groups.engineer.roles.senior-engineer.disable_tools",
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
        role_groups:
          engineer:
            roles:
              senior-engineer:
                enabled: false
                enable: true
        "#,
    )
    .expect("write");

    let error = load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflicting aliases");

    assert!(
        error.to_string().contains("enabled")
            && error.to_string().contains("enable")
            && error.to_string().contains("senior-engineer"),
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
    std::fs::set_permissions(&drop_dir, std::fs::Permissions::from_mode(0o000))
        .expect("chmod unreadable");

    let error = load_cli_settings_in(&dirs_with_config(dir)).expect_err("unreadable drop-in dir");

    std::fs::set_permissions(&drop_dir, std::fs::Permissions::from_mode(0o700))
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
        r#"{ show_diff: true, show_thinking: false, show_turn_stats: true, redraw_counter: true, show_ui_io: true, show_tools: "compact", show_messages: "self-full", notice_level: "warning", show_status: "minimal", show_prompt_scroll_indicator: false }"#,
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
            show_ui_io: true,
            show_tools: ShowTools::Compact,
            show_messages: ShowMessages::SelfFull,
            notice_level: tau_proto::NoticeLevel::Warning,
            show_status: ShowStatus::Minimal,
            show_prompt_scroll_indicator: false,
        }
    );
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
            defaultRole: "manager",
            agents: { idTemplate: "legacy-{{random_alphanumeric 4}}" },
            roleGroups: {
                engineer: {
                    promptFragments: [{ name: "legacy.group", priority: 80, text: "group" }],
                    roles: {
                        "senior-engineer": {
                            enableTools: ["web_search"],
                            promptFragments: [{ name: "legacy.role", priority: 90, text: "role" }],
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
    let senior = settings.roles.get("senior-engineer").expect("senior role");
    assert!(
        senior
            .enable_tools
            .iter()
            .any(|tool| tool.as_str() == "web_search")
    );
    assert!(
        senior
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name.as_str() == "legacy.group")
    );
    assert!(
        senior
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
            extensions: {
                "core-shell": { config: { working_directory: "/from-file" } },
                "std-websearch": { enable: true },
            },
        }"#,
    )
    .expect("write");

    let overrides = [
        HarnessConfigCliOverride::from_str("session_retention_days=3").expect("override"),
        HarnessConfigCliOverride::from_str(
            "extensions.core-shell.config.working_directory=/from-cli",
        )
        .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.std-websearch.enable=false")
            .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.core-shell.command=[\"tau\", \"ext\"]")
            .expect("override"),
    ];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(s.session_retention_days, 3);
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
        "role_groups.engineer.roles.senior-engineer.effort=low",
    )
    .expect("override")];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(
        s.roles["senior-engineer"].effort,
        Some(tau_proto::Effort::Low)
    );
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
        "role_groups.engineer.roles.senior-engineer.enabled=false",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert!(!settings.roles.contains_key("senior-engineer"));
}

/// Ensures CLI overrides using legacy top-level aliases still target canonical
/// settings.
#[test]
fn harness_config_cli_overrides_normalize_legacy_top_level_aliases() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "roleGroups.engineer.roles.senior-engineer.effort=low",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert_eq!(
        settings.roles["senior-engineer"].effort,
        Some(tau_proto::Effort::Low)
    );
}

/// Ensures CLI overrides reject alias/canonical conflicts within the same
/// synthetic layer.
#[test]
fn harness_config_cli_overrides_reject_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    let overrides = [
        HarnessConfigCliOverride::from_str("defaultRole=manager").expect("legacy override"),
        HarnessConfigCliOverride::from_str("default_role=senior-engineer")
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
        "role_groups.engineer.roles.senior-engineer={enabled: false}",
    )
    .expect("override")];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");

    assert!(!settings.roles.contains_key("senior-engineer"));
}

/// Ensures YAML map-valued CLI overrides reject alias/canonical conflicts
/// inside the value.
#[test]
fn harness_config_cli_overrides_reject_map_value_alias_conflicts() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    let overrides = [HarnessConfigCliOverride::from_str(
        "role_groups.engineer.roles.senior-engineer={enabled: false, enable: true}",
    )
    .expect("override")];

    let error =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect_err("conflicting map aliases");

    assert!(
        error.to_string().contains("enabled")
            && error.to_string().contains("enable")
            && error.to_string().contains("senior-engineer"),
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

/// Ensures role tool allow/deny lists load into effective role settings.
#[test]
fn harness_settings_load_role_tool_lists() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
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

/// Ensures group defaults can clear fields on roles inherited from lower
/// layers.
#[test]
fn harness_role_group_defaults_can_clear_existing_role_fields() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
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
        role_groups:
          custom:
            description: null
            prompt_override: null
        "#,
    )
    .expect("write dropin");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = settings.roles.get("reviewer").expect("reviewer role");

    assert_eq!(reviewer.description, None);
    assert_eq!(reviewer.prompt_override, None);
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
        settings.roles["senior-engineer"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
    );
    assert_eq!(
        settings.roles["custom"].disable_tools,
        vec![tau_proto::ToolName::new("shell")]
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

/// Ensures group-level tool defaults update inherited roles without relisting
/// each role.
#[test]
fn harness_settings_load_role_group_default_tool_overrides_without_relisting_roles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            role_groups: {
                engineer: { enable_tools: ["email_list_recent"], disable_tools: ["email"] },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    for role_name in ["senior-engineer", "junior-engineer", "staff-engineer"] {
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

/// Ensures user config may define a new role group with its own roles.
#[test]
fn harness_settings_allow_new_role_group() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            role_groups: {
                reviewers: {
                    disable_tools: ["email"],
                    roles: {
                        reviewer: { effort: "high" },
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
            role_groups: {
                reviewers: {
                    roles: {
                        senior-engineer: { effort: "high" },
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
            .contains("role `senior-engineer` appears in multiple role_groups"),
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
            role_groups: {
                engineer: {
                    roles: {
                        senior-engineer: { staleRoleField: true },
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
            prompt_fragments: [
                { name: "global.typo", priority: 50, text: "x", staleFragmentField: true },
            ],
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
            role_groups: {
                manager: {
                    roles: {
                        manager: { enable: false },
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
            RoleCliOverride::Enable("micro-manager".to_owned()),
            RoleCliOverride::Disable("micro-manager".to_owned()),
        ],
    )
    .expect("load");

    assert!(!s.roles.contains_key("micro-manager"));
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
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
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
            prompt_fragments: [
                { name: "global.local", priority: 60, text: "Local global instruction." },
            ],
            role_groups: {
                manager: {
                    roles: {
                        "micro-manager": { prompt_fragments: [{ name: "manager.local", priority: 170, text: "Local manager instruction." }] },
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
            prompt_fragments: [
                { name: "global.drop-in", priority: 70, text: "Drop-in global instruction." },
            ],
            role_groups: {
                manager: {
                    roles: {
                        "micro-manager": { prompt_fragments: [{ name: "manager.drop-in", priority: 180, text: "Drop-in manager instruction." }] },
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
    let manager = &s.roles["micro-manager"];
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| { fragment.text.as_str().contains("delegating to sub-agents") })
    );
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

/// Ensures global prompt fragments are appended to every effective role prompt.
#[test]
fn harness_global_prompt_fragments_apply_to_all_roles() {
    // Top-level prompt fragments are role-independent style/context hooks. They
    // must apply to built-in roles and roles created by user config without
    // duplicating the same fragment when a drop-in repeats it exactly.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
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
        }"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("mkdir harness.d");
    std::fs::write(
        dir.join("harness.d").join("01-repeat.yaml"),
        r#"{
            prompt_fragments: [
                { name: "global.simple", priority: 65, text: "Use simple words." },
            ],
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
    for role_name in ["senior-engineer", "micro-manager", "custom"] {
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
            role_groups: {
                engineer: {
                    roles: {
                        engineer: { model: "openai/gpt-5.5", tools: ["read"] },
                        custom: { description: "Custom local role", effort: "medium", disable_tools: ["shell"] },
                    },
                },
                manager: {
                    roles: {
                        "micro-manager": { model: "openai/gpt-5.5" },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("engineer"));
    assert!(s.roles.contains_key("micro-manager"));
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

    let manager = &s.roles["micro-manager"];
    assert_eq!(
        manager.description.as_deref(),
        Some(
            "Role focused on splitting and delegation of tasks to other sub-agents via micro-management."
        )
    );
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("delegating to sub-agents"))
    );
}

/// Ensures partial manager role overrides preserve built-in prompt fragments.
#[test]
fn harness_manager_partial_override_keeps_built_in_prompt_fragments() {
    // Built-in manager prompt fragments are stored in the built-in harness
    // config, so a user can partially override manager settings without
    // accidentally disabling delegation prompt behavior.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            role_groups: {
                manager: {
                    roles: {
                        "micro-manager": { model: "openai/gpt-5.5" },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let manager = &s.roles["micro-manager"];
    assert!(manager.prompt_fragments.iter().any(|fragment| {
        fragment
            .text
            .as_str()
            .contains("self-contained instructions")
    }));
    assert_eq!(
        manager.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
    );
}

/// Ensures manager role prompt fragments append to, rather than replace,
/// built-in fragments.
#[test]
fn harness_manager_prompt_fragments_extend_built_in_prompt_fragments() {
    // User-provided role prompt fragments are added to the built-in role
    // fragments so partial manager customization does not disable delegation
    // instructions.
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            role_groups: {
                manager: {
                    roles: {
                        "micro-manager": { prompt_fragments: [{ name: "manager.custom", priority: 100, text: "Custom manager prompt." }] },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let manager = &s.roles["micro-manager"];
    assert!(manager.prompt_fragments.iter().any(|fragment| {
        fragment
            .text
            .as_str()
            .contains("self-contained instructions")
    }));
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str() == "Custom manager prompt.")
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

/// Ensures the embedded built-in roles load with the expected manager prompt
/// content.
#[test]
fn harness_built_in_roles_load_from_json_with_manager_prompt() {
    // Built-in role defaults live in built-in.harness.yaml. Manager has a
    // visible orchestration prompt there. Engineer roles share a lightweight
    // instruction prompt.
    let s = HarnessSettings::built_in();
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
    assert_eq!(
        s.role_groups
            .iter()
            .map(|group| (group.name.clone(), group.roles.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                "engineer".to_owned(),
                vec![
                    "junior-engineer".to_owned(),
                    "senior-engineer".to_owned(),
                    "staff-engineer".to_owned(),
                ],
            ),
            ("manager".to_owned(), vec!["micro-manager".to_owned()]),
        ]
    );
    let junior_engineer = &s.roles["junior-engineer"];
    assert_eq!(junior_engineer.effort, Some(tau_proto::Effort::Low));
    let senior_engineer = &s.roles["senior-engineer"];
    assert_eq!(
        senior_engineer.prompt_fragments[0].priority,
        PromptPriority::new(15)
    );
    assert!(
        senior_engineer.prompt_fragments[0]
            .text
            .contains("Trust the `<instructions>`")
    );
    assert!(!s.roles.contains_key("assistant"));
    let staff_engineer = &s.roles["staff-engineer"];
    assert_eq!(staff_engineer.effort, Some(tau_proto::Effort::High));
    assert!(
        staff_engineer
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.contains("Trust the `<instructions>`"))
    );
    let manager = &s.roles["micro-manager"];
    let prompt = manager
        .prompt_fragments
        .first()
        .expect("manager prompt fragment")
        .text
        .as_str();
    assert_eq!(manager.prompt_fragments[0].priority, PromptPriority::new(5));
    assert_eq!(manager.prompt_fragments[1].priority, PromptPriority::new(6));
    assert!(prompt.contains("You are a planning and orchestration agent"));
    assert!(prompt.contains("delegating to sub-agents"));
    assert!(prompt.contains("available sub-task roles list"));
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
            role_groups: {
                coding: {
                    roles: {
                        custom: { effort: "medium", tools: ["read"] },
                    },
                },
                manager: {
                    roles: {
                        "micro-manager": { model: "openai/gpt-5.5" },
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
    let manager = &s.roles["micro-manager"];
    assert!(
        manager
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.text.as_str().contains("delegating to sub-agents"))
    );
    assert_eq!(
        manager.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-5.5")
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
            role_groups: {
                engineer: {
                    order: 40,
                    roles: {
                        "staff-engineer": { order: null },
                        "senior-engineer": { order: 20 },
                        "junior-engineer": {},
                        "custom-engineer": {},
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert_eq!(s.roles["junior-engineer"].order, Some(40));
    assert_eq!(s.roles["senior-engineer"].order, Some(20));
    assert_eq!(s.roles["staff-engineer"].order, None);
    assert_eq!(s.roles["custom-engineer"].order, Some(40));
}

/// Ensures harness custom prompts parse from map syntax, sort by id, and are
/// available by stable id for the CLI `/prompt <id>` command.
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
/// producing ambiguous or unreachable `/prompt <id>` commands.
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
            role_groups: {
                coding: { roles: { engineer: {} } },
                review: { roles: { engineer: {} } },
            },
        }"#,
    )
    .expect("write");

    let err = load_harness_settings_in(&dirs_with_config(dir)).expect_err("duplicate role");
    assert!(err.to_string().contains("appears in multiple role_groups"));
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
    assert!(harness.roles.contains_key("junior-engineer"));
    assert!(harness.roles.contains_key("senior-engineer"));
    assert!(harness.roles.contains_key("micro-manager"));
    assert_eq!(harness.default_role.as_deref(), Some("senior-engineer"));
    assert!(!harness.roles.contains_key("assistant"));
    assert!(harness.roles.contains_key("staff-engineer"));
    assert_eq!(
        harness.roles["staff-engineer"].effort,
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
            default_role: "senior-engineer",
            role_groups: {
                engineer: {
                    roles: {
                        "junior-engineer": { enable: false },
                        "senior-engineer": { enable: false },
                        "staff-engineer": { enable: false },
                    },
                },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.roles.contains_key("junior-engineer"));
    assert!(!s.roles.contains_key("senior-engineer"));
    assert!(!s.roles.contains_key("staff-engineer"));
    assert!(!s.roles.contains_key("assistant"));
    assert_eq!(s.default_role.as_deref(), Some("senior-engineer"));
    assert_eq!(
        s.role_groups
            .iter()
            .map(|group| (group.name.as_str(), group.roles.as_slice()))
            .collect::<Vec<_>>(),
        vec![("manager", &["micro-manager".to_owned()][..]),]
    );
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
            role_groups: {
                legacy: {
                    enabled: false,
                    roles: {
                        old_on: { enabled: true },
                        old_off: {},
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
        role_groups:
          engineer:
            roles:
              senior-engineer:
                enabled: false
        "#,
    )
    .expect("write");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");

    assert!(!settings.roles.contains_key("senior-engineer"));
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
        r#"{ role_groups: { engineer: { roles: { "staff-engineer": { enable: false } } } } }"#,
    )
    .expect("write base");
    std::fs::write(
        dir.join("harness.d/10-enable.yaml"),
        r#"{ role_groups: { engineer: { roles: { "staff-engineer": { enable: true, effort: "xhigh" } } } } }"#,
    )
    .expect("write drop-in");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(s.roles.contains_key("staff-engineer"));
    assert_eq!(s.roles["staff-engineer"].enable, Some(true));
    assert!(
        s.role_groups.iter().any(|group| group.name == "engineer"
            && group.roles.iter().any(|role| role == "staff-engineer"))
    );
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
    let state_dir = std::path::Path::new("/tmp/tau-state");
    for name in ["a", "a_b", "x9", "std-email"] {
        assert_eq!(
            extension_state_dir_of(state_dir, name).expect("safe extension name"),
            state_dir.join("ext").join(name)
        );
    }

    for name in ["", "../x", "a/b", "/tmp/x", ".", "..", "foo.bar"] {
        assert!(
            extension_state_dir_of(state_dir, name).is_err(),
            "{name:?} must be rejected"
        );
    }
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
