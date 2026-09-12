use std::str::FromStr;
use std::{fs as path_std_fs, path as path_std_path, process as path_std_process};

use tempfile::TempDir;

use super::*;
use crate::settings as path_crate_settings;

/// Built-in logical web policy selects cached exact-route search first and
/// keeps caller-directed fetch external.
#[test]
fn built_in_web_tools_policy_matches_native_first_contract() {
    let settings = HarnessSettings::built_in();
    let policy = &settings.roles["engineer"].web_tools;
    let (_, native) = policy
        .search()
        .candidates()
        .find(|(name, _)| *name == "native")
        .expect("native");
    assert!(matches!(
        native,
        WebToolCandidate::ModelProvider {
            priority: 10,
            access: WebSearchAccess::Cached,
            context_size: None,
            ..
        }
    ));
    let (_, fetch) = policy
        .fetch()
        .candidates()
        .find(|(name, _)| *name == "external")
        .expect("external fetch");
    assert!(matches!(
        fetch,
        WebToolCandidate::Tool {
            priority: 20, tool, ..
        }
            if tool.as_str() == "websearch_hybrid_fetch"
    ));
    let encoded = serde_json::to_value(policy).expect("serialize effective policy");
    let decoded: WebToolsPolicy =
        serde_json::from_value(encoded).expect("deserialize validated effective policy");
    assert_eq!(&decoded, policy);
}

/// Candidate maps merge field-by-field through agent, group, and role layers;
/// explicit null clears inherited context size and domain restrictions.
#[test]
fn web_tools_role_merge_preserves_named_candidate_null_semantics() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  web_tools:
    allowed_domains: [rust-lang.org]
    search:
      candidates:
        native: { access: live }
  role_groups:
    engineer:
      web_tools:
        search:
          candidates:
            native: { context_size: high }
      roles:
        engineer:
          web_tools:
            allowed_domains: null
            search:
              candidates:
                native: { context_size: null }
"#,
    )
    .expect("write config");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load config");
    let policy = &settings.roles["engineer"].web_tools;
    assert_eq!(policy.allowed_domains(), None);
    let (_, native) = policy
        .search()
        .candidates()
        .find(|(name, _)| *name == "native")
        .expect("native");
    assert!(matches!(
        native,
        WebToolCandidate::ModelProvider {
            priority: 10,
            access: WebSearchAccess::Live,
            context_size: None,
            ..
        }
    ));
}

/// Domain and candidate validation rejects unsafe or ambiguous policy at the
/// authored path rather than discovering it during provider delivery.
#[test]
fn web_tools_reject_malformed_domains_and_candidate_shapes() {
    let cases = vec![
        (
            "allowed_domains: [https://example.com]".to_owned(),
            "expected a lowercase DNS domain name",
        ),
        (
            "allowed_domains: [example.com, example.com]".to_owned(),
            "duplicate domain `example.com`",
        ),
        (
            format!(
                "allowed_domains: [{}]",
                (0..101)
                    .map(|index| format!("d{index}.example"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "at most 100 domains",
        ),
        (
            "search:\n      candidates:\n        broken: { enable: true, priority: 1 }".to_owned(),
            "broken.kind: field is required",
        ),
        (
            "search:\n      candidates:\n        broken: { enable: true, priority: 1, kind: tool, tool: read, access: live }".to_owned(),
            "tool candidates cannot set access or context_size",
        ),
        (
            "fetch:\n      candidates:\n        broken: { enable: true, priority: 1, kind: model_provider }".to_owned(),
            "model_provider does not implement logical fetch",
        ),
        (
            "search:\n      candidates: {}".to_owned(),
            "candidate map must not be empty",
        ),
    ];
    for (policy, expected) in cases {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(
            td.path().join("harness.yaml"),
            format!("agents:\n  web_tools:\n    {policy}\n"),
        )
        .expect("write config");
        let error =
            load_harness_settings_in(&dirs_with_config(td.path())).expect_err("invalid web policy");
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?}, got {error}"
        );
    }
}

/// Profile replay merges same-name candidates left-to-right; disabled
/// candidates remain disabled and equal priorities retain deterministic names.
#[test]
fn web_tools_profiles_merge_candidates_and_disable_native() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  restricted:
    agents:
      web_tools:
        allowed_domains: [example.com]
        search:
          candidates:
            native: { enable: false }
            zeta: { enable: true, priority: 20, kind: tool, tool: alternate_search }
"#,
    )
    .expect("write profile");
    let selection = profile_selection("restricted");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&selection),
        &[],
        &[],
    )
    .expect("load selected profile");
    let policy = &settings.roles["engineer"].web_tools;
    assert_eq!(
        policy.allowed_domains(),
        Some(["example.com".to_owned()].as_slice())
    );
    let candidates = policy.search().candidates().collect::<Vec<_>>();
    assert!(matches!(
        candidates.iter().find(|(name, _)| *name == "native"),
        Some((_, WebToolCandidate::ModelProvider { enable: false, .. }))
    ));
    assert!(candidates.windows(2).all(|pair| pair[0].0 < pair[1].0));
}
/// Provider and model aliases resolve independently after agent/group/role
/// replay, including exact model suffixes that themselves contain slashes.
#[test]
fn model_reference_aliases_resolve_final_effective_role_models() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
aliases:
  providers:
    current: preferred
    preferred: codex-work
  models:
    fast: vendor/qwen-fast
agents:
  model: current/fast
  role_groups:
    alias-test:
      model: current/fast
      roles:
        inherited: {}
        exact:
          model: current/vendor/qwen-fast
        case-sensitive:
          model: current/Fast
        no-prefix:
          model: current/fastest
"#,
    )
    .expect("write alias config");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("resolve aliases");
    assert_eq!(
        settings.roles["inherited"]
            .model
            .as_ref()
            .map(ToString::to_string),
        Some("codex-work/vendor/qwen-fast".to_owned())
    );
    assert_eq!(
        settings.roles["exact"]
            .model
            .as_ref()
            .map(ToString::to_string),
        Some("codex-work/vendor/qwen-fast".to_owned())
    );
    assert_eq!(
        settings.roles["case-sensitive"]
            .model
            .as_ref()
            .map(ToString::to_string),
        Some("codex-work/Fast".to_owned())
    );
    assert_eq!(
        settings.roles["no-prefix"]
            .model
            .as_ref()
            .map(ToString::to_string),
        Some("codex-work/fastest".to_owned())
    );
}

/// Selected profiles can retarget lower-layer model references, while a final
/// dedicated-style config layer remains the highest-precedence alias value.
#[test]
fn profile_and_final_alias_layers_retarget_lower_model_references() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
aliases:
  providers:
    subscription: personal
agents:
  model: subscription/gpt
profiles:
  work:
    aliases:
      providers:
        subscription: work
"#,
    )
    .expect("write profile aliases");
    let profile = profile_selection("work");
    let cli = [
        HarnessConfigCliOverride::from_str("aliases.providers={subscription: emergency}")
            .expect("final alias override"),
    ];

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &cli,
    )
    .expect("load layered aliases");
    assert!(settings.roles.values().all(|role| {
        role.model
            .as_ref()
            .is_none_or(|model| model.provider == "emergency")
    }));
}

/// Identity mappings terminate alias chains, while every non-identity cycle is
/// rejected even when no configured role refers to it.
#[test]
fn alias_graphs_accept_identity_resets_and_reject_unused_cycles() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "aliases:\n  providers:\n    literal: target\nagents:\n  model: literal/model\n",
    )
    .expect("write lower alias");
    let reset = [
        HarnessConfigCliOverride::from_str("aliases.providers={literal: literal}")
            .expect("identity override"),
    ];
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        None,
        &[],
        &reset,
    )
    .expect("identity is terminal");
    assert!(settings.roles.values().all(|role| {
        role.model
            .as_ref()
            .is_none_or(|model| model.provider == "literal")
    }));

    std::fs::write(
        td.path().join("harness.yaml"),
        "aliases:\n  models:\n    a: b\n    b: a\n",
    )
    .expect("write cycle");
    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("unused cycle must fail");
    assert_eq!(error.to_string(), "model alias cycle: a -> b -> a");
}

/// Environment aliases use strict JSON objects, and later repeated CLI
/// operations replace matching environment entries without path interpretation.
#[test]
fn alias_environment_and_cli_operations_have_typed_last_wins_precedence() {
    let provider_cli = [
        "current=second".parse().expect("first provider override"),
        "current=final.name"
            .parse()
            .expect("last provider override"),
    ];
    let model_cli = ["fast=org/qwen".parse().expect("model override")];
    let overrides = model_reference_alias_config_overrides(ModelReferenceAliasSources {
        provider_environment: Some(r#"{"current":"environment"}"#.into()),
        model_environment: Some(r#"{"other":"model"}"#.into()),
        provider_cli: &provider_cli,
        model_cli: &model_cli,
    })
    .expect("parse alias sources");

    assert_eq!(overrides.len(), 2);
    assert!(overrides[0].raw_value.contains(r#""current":"final.name""#));
    assert!(overrides[1].raw_value.contains(r#""fast":"org/qwen""#));
    assert!(
        model_reference_alias_config_overrides(ModelReferenceAliasSources {
            provider_environment: Some("current=bad".into()),
            ..Default::default()
        })
        .expect_err("non-JSON environment must fail")
        .to_string()
        .contains(TAU_PROVIDER_ALIASES_ENV)
    );
}

/// Alias targets need not be currently published, and a later cold load uses
/// the newly configured canonical target without retaining prior provenance.
#[test]
fn alias_targets_are_syntax_only_and_cold_reloads_use_current_mapping() {
    let td = TempDir::new().expect("tempdir");
    let write_target = |target: &str| {
        std::fs::write(
            td.path().join("harness.yaml"),
            format!(
                "aliases:\n  providers:\n    current: {target}\nagents:\n  model: current/unknown\n"
            ),
        )
        .expect("write alias target");
    };
    write_target("not-published-one");
    let first =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("accept unknown target");
    write_target("not-published-two");
    let second =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("reload changed target");
    assert!(first.roles.values().all(|role| {
        role.model
            .as_ref()
            .is_none_or(|model| model.provider == "not-published-one")
    }));
    assert!(second.roles.values().all(|role| {
        role.model
            .as_ref()
            .is_none_or(|model| model.provider == "not-published-two")
    }));
}

/// Provider grammar and nonempty model names fail during config loading with
/// their alias namespace visible instead of becoming unresolved runtime names.
#[test]
fn alias_config_rejects_invalid_provider_and_empty_model_names() {
    for (yaml, expected) in [
        (
            "aliases:\n  providers:\n    bad/name: target\n",
            "provider name",
        ),
        ("aliases:\n  models:\n    \"\": target\n", "model name"),
        ("aliases:\n  models:\n    source: \"\"\n", "model name"),
    ] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(td.path().join("harness.yaml"), yaml).expect("write invalid alias");
        let error = load_harness_settings_in(&dirs_with_config(td.path()))
            .expect_err("invalid alias name must fail");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

/// Proves portable and mutable provider helpers use the same instance-qualified
/// shape beneath their distinct TauDirs roots.
#[test]
fn provider_profile_roots_are_disjoint_and_instance_qualified() {
    let config = path_std_path::Path::new("/config/tau");
    let state = path_std_path::Path::new("/state/tau");

    assert_eq!(
        extension_provider_config_dir_of(config, "provider-work").expect("config path"),
        config.join("providers/provider-work")
    );
    assert_eq!(
        extension_provider_settings_dir_of(state, "provider-work").expect("state path"),
        state.join("providers/provider-work")
    );
}

/// Ensures the emergency state-access override accepts only its two supported
/// exact tokens and rejects the removed ambient-writable mode.
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
    assert_eq!(parse_tau_state_access_env(None).expect("absent"), None);
    for invalid in ["", "Hidden", "read-only", "legacy", "legacy ", "all"] {
        assert!(
            parse_tau_state_access_env(Some(invalid.into())).is_err(),
            "{invalid:?} must fail closed"
        );
    }
}

/// Provider cache refresh stays disabled with the approved finite default.
#[test]
fn provider_cache_refresh_defaults_disabled() {
    let settings = HarnessSettings::built_in();
    assert_eq!(
        settings.provider_cache_refresh,
        ProviderCacheRefresh {
            enabled: false,
            max_idle_seconds: ProviderCacheMaxIdle::new(300).expect("valid default"),
        }
    );
}
/// Sparse drop-ins merge cache-refresh fields without resetting the idle bound.
#[test]
fn provider_cache_refresh_merges_recursively() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "provider_cache_refresh:\n  max_idle_seconds: 42\n",
    )
    .expect("write base");
    std::fs::create_dir(td.path().join("harness.d")).expect("drop-in directory");
    std::fs::write(
        td.path().join("harness.d/10-enable.yaml"),
        "provider_cache_refresh:\n  enabled: true\n",
    )
    .expect("write drop-in");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load cache refresh");
    assert_eq!(
        settings.provider_cache_refresh,
        ProviderCacheRefresh {
            enabled: true,
            max_idle_seconds: ProviderCacheMaxIdle::new(42).expect("valid test bound"),
        }
    );
}

/// The approved idle bound validates even when refresh is disabled.
#[test]
fn provider_cache_refresh_rejects_out_of_range_idle() {
    for seconds in [0, 86_401] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(
            td.path().join("harness.yaml"),
            format!("provider_cache_refresh:\n  enabled: false\n  max_idle_seconds: {seconds}\n"),
        )
        .expect("write config");
        let error = load_harness_settings_in(&dirs_with_config(td.path()))
            .expect_err("out-of-range idle must fail");
        assert!(error.to_string().contains(
            "provider_cache_refresh.max_idle_seconds must be between 1 and 86400 seconds inclusive"
        ));
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
            .to_string(),
        "cli"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, None, Some("environment".into()))
            .expect("valid environment profile")
            .expect("environment selection")
            .to_string(),
        "environment"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, None, None)
            .expect("configured fallback profile")
            .expect("configured selection")
            .to_string(),
        "configured"
    );
    assert_eq!(
        selected_profile_in_from_sources(&dirs, Some("configured"), None)
            .expect("explicit default profile")
            .expect("explicit selection")
            .to_string(),
        "configured"
    );
    assert_eq!(
        selected_profile_from_sources(None, None).expect("absent selection"),
        None
    );
}

/// Ensures profile selection accepts only nonempty comma-separated names while
/// retaining left-to-right duplicates instead of silently changing a request.
#[test]
fn selected_profile_rejects_invalid_names() {
    assert_eq!(
        ProfileSelection::parse(" xyz,\tfoo ")
            .expect("space/tab around selection names")
            .to_string(),
        "xyz,foo"
    );
    assert_eq!(
        ProfileSelection::parse("xyz,xyz")
            .expect("duplicate selection")
            .names()
            .len(),
        2
    );
    for name in [" xyz", "xyz,foo"] {
        assert!(
            ProfileSelection::try_from(ProfileName::parse(name).expect("profile name")).is_err(),
            "{name:?} must not become a different one-item selection"
        );
    }
    for selection in ["", " ", "\t", "\n", "\u{a0}", ",xyz", "xyz,", "xyz,,foo"] {
        assert!(
            ProfileSelection::parse(selection).is_err(),
            "{selection:?} must reject an empty profile item"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        assert!(selected_profile_from_sources(None, Some(OsString::from_vec(vec![0xff]))).is_err());
    }
}

/// Ensures the built-in base, user base, and every named profile form one
/// ordered stack, including duplicate profile applications and unknown names.
#[test]
fn selected_profiles_apply_all_layers_in_exact_order() {
    let td = TempDir::new().expect("tempdir");
    let dirs = dirs_with_config(td.path());
    assert_eq!(
        load_harness_settings_in(&dirs)
            .expect("built-in base")
            .tau_state_access,
        TauStateAccess::ReadOnly
    );
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
tau_state_access: read_only
agents:
  effort: 0.25
  role_groups:
    base:
      roles:
        layered: {}
extensions:
  core-shell:
    config:
      nested:
        user: base
        shared: user
profiles:
  xyz:
    tau_state_access: read_only
    agents:
      effort: increase:0.25
    extensions:
      core-shell:
        config:
          nested:
            xyz: selected
            shared: xyz
  foo:
    tau_state_access: hidden
    extensions:
      core-shell:
        config:
          nested:
            foo: selected
            shared: foo
"#,
    )
    .expect("write user base and profiles");

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs,
        Some(&profile_selection("xyz,foo")),
        &[],
        &[],
    )
    .expect("load selected stack");
    assert_eq!(settings.tau_state_access, TauStateAccess::Hidden);
    assert_eq!(
        settings.extensions["core-shell"].config,
        Some(serde_json::json!({
            "nested": {
                "user": "base",
                "xyz": "selected",
                "foo": "selected",
                "shared": "foo"
            }
        }))
    );

    let duplicated = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs,
        Some(&profile_selection("xyz,xyz")),
        &[],
        &[],
    )
    .expect("replay duplicate profile");
    assert_eq!(
        duplicated.roles["layered"].effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
    );

    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs,
        Some(&profile_selection("xyz,missing,foo")),
        &[],
        &[],
    )
    .expect_err("unknown profile in ordered selection");
    assert_eq!(
        error.to_string(),
        "unknown configuration profile: `missing`"
    );
}

fn dirs_with_config(dir: &std::path::Path) -> TauDirs {
    TauDirs {
        config_dir: Some(dir.to_path_buf()),
        state_dir: None,
    }
}

/// Removed harness spellings must fail in both YAML files and dotted CLI
/// overrides so no input path silently preserves the retired compatibility.
#[test]
fn removed_harness_config_spellings_are_rejected() {
    let cases = [
        ("sessionRetention", "1d"),
        ("agentRetention", "1d"),
        ("diagnosticRetention", "1d"),
        ("interSession", "{}"),
        ("inter_session.receiver.autoStart", "false"),
        ("customPrompts", "{}"),
        ("toolPolicy", "{}"),
        ("showIntroductionNotice", "false"),
        ("waitTimeoutMinimumMinutes", "1"),
        ("waitTimeoutMaximumMinutes", "2"),
        ("agentWatchRetryNotificationThreshold", "2"),
        ("notificationDelivery", "{}"),
        ("notification_delivery.idle.idleMs", "1"),
        ("notification_delivery.idle.waitAnyMs", "1"),
        ("notification_delivery.idle.waitToolMs", "1"),
        ("extensions.core-shell.toolPrefix", "work"),
        ("extensions.core-shell.enabled", "false"),
        ("agents.enabled", "false"),
        ("agents.defaultRole", "engineer"),
        ("agents.idTemplate", "agent-{{ sequence }}"),
        ("agents.displayNameTemplate", "Agent"),
        ("agents.promptFragments", "[]"),
        ("agents.requiredSkills", "[]"),
        ("agents.thinkingSummary", "auto"),
        ("agents.serviceTier", "fast"),
        ("agents.inferenceCompaction", "provider_default"),
        ("agents.contextSizeAlerts", "{}"),
        ("agents.roleGroups", "{}"),
        ("agents.webTools", "{}"),
        ("agents.role_groups.engineer.enabled", "true"),
        ("agents.role_groups.engineer.interSessionReceiver", "false"),
        ("agents.role_groups.engineer.interSessionAutoStart", "false"),
        ("agents.role_groups.engineer.thinkingSummary", "auto"),
        ("agents.role_groups.engineer.serviceTier", "fast"),
        (
            "agents.role_groups.engineer.inferenceCompaction",
            "provider_default",
        ),
        ("agents.role_groups.engineer.contextSizeAlerts", "{}"),
        ("agents.role_groups.engineer.promptFragments", "[]"),
        ("agents.role_groups.engineer.promptOverride", "null"),
        ("agents.role_groups.engineer.disableToolTags", "[]"),
        ("agents.role_groups.engineer.enableToolTags", "[]"),
        ("agents.role_groups.engineer.disableToolGroups", "[]"),
        ("agents.role_groups.engineer.enableToolGroups", "[]"),
        ("agents.role_groups.engineer.disableTools", "[]"),
        ("agents.role_groups.engineer.enableTools", "[]"),
        ("agents.role_groups.engineer.requiredSkills", "[]"),
        ("agents.role_groups.engineer.webTools", "{}"),
        ("agents.web_tools.allowedDomains", "[]"),
        (
            "agents.web_tools.search.candidates.native.contextSize",
            "high",
        ),
        ("tool_policy.rules.test.enabled", "false"),
        ("agents.compaction", "disabled"),
        ("agents.inference_compaction", "providerDefault"),
        ("agents.compactions.default.threshold", "contextLimitSafe"),
        ("agents.compactions.default.when.at", "outerTurnFinished"),
    ];

    for (key, raw_value) in cases {
        let value = serde_yaml_ng::from_str(raw_value).expect("parse test value");
        let nested = nested_harness_override_value(key, value);
        let yaml = serde_yaml_ng::to_string(&nested).expect("serialize test config");
        let file_dir = TempDir::new().expect("tempdir");
        std::fs::write(file_dir.path().join("harness.yaml"), yaml).expect("write test config");
        assert!(
            load_harness_settings_in(&dirs_with_config(file_dir.path())).is_err(),
            "file spelling unexpectedly accepted: {key}"
        );

        let cli_dir = TempDir::new().expect("tempdir");
        let override_ = HarnessConfigCliOverride::from_str(&format!("{key}={raw_value}"))
            .expect("parse CLI override");
        assert!(
            load_harness_settings_with_cli_overrides_in(
                &dirs_with_config(cli_dir.path()),
                &[],
                &[override_],
            )
            .is_err(),
            "CLI spelling unexpectedly accepted: {key}"
        );
    }
}

/// The retired `show_tools: on` state value must require migration to `full`.
#[test]
fn cli_state_rejects_removed_show_tools_on_value() {
    serde_json::from_value::<CliSettings>(serde_json::json!({ "show_tools": "on" }))
        .expect_err("removed show_tools value must fail");
}

fn profile_selection(value: &str) -> ProfileSelection {
    ProfileSelection::parse(value).expect("valid profile selection")
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

/// Ensures nullable retention policies are disabled independently by default.
#[test]
fn session_and_agent_retention_default_to_disabled() {
    let settings = HarnessSettings::built_in();
    assert_eq!(settings.session_retention(), None);
    assert_eq!(settings.agent_retention(), None);
    assert_eq!(settings.artifact_retention(), None);
}

/// Artifact policy is independent, explicitly nullable, and uses the same
/// validated duration grammar rather than borrowing diagnostic defaults.
#[test]
fn artifact_retention_is_independent_and_nullable() {
    let mut json: serde_json::Value =
        serde_yaml_ng::from_str(BUILT_IN_HARNESS_YAML).expect("built-in YAML");
    json["artifact_retention"] = serde_json::json!("2h");
    let settings: HarnessSettings =
        serde_json::from_value(json.clone()).expect("artifact retention settings");
    assert_eq!(
        settings.artifact_retention(),
        Some(Duration::from_secs(7200))
    );
    assert_eq!(settings.session_retention(), None);
    json["artifact_retention"] = serde_json::Value::Null;
    let disabled: HarnessSettings =
        serde_json::from_value(json.clone()).expect("disabled artifact retention");
    assert_eq!(disabled.artifact_retention(), None);
    for invalid in ["0s", "-1d", "1d2h", "1.5h"] {
        json["artifact_retention"] = serde_json::json!(invalid);
        assert!(serde_json::from_value::<HarnessSettings>(json.clone()).is_err());
    }
}

/// Ensures non-authoritative diagnostic cleanup defaults to thirty days and
/// can be disabled independently from whole-session retention.
#[test]
fn diagnostic_retention_has_independent_default_and_disable() {
    let built_in = HarnessSettings::built_in();
    assert_eq!(
        built_in.diagnostic_retention(),
        Some(std::time::Duration::from_secs(30 * 24 * 60 * 60))
    );

    let disabled = HarnessSettings {
        diagnostic_retention: None,
        ..built_in
    };
    assert_eq!(disabled.diagnostic_retention(), None);
}

/// Ensures the whole-session outbound policy loads through the ordinary harness
/// schema and preserves the explicit empty-allowlist deny-all distinction.
#[test]
fn inter_session_project_root_policy_loads_with_empty_allowlist_semantics() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
inter_session:
  allow_project_roots: []
  deny_project_roots: [/srv/private/**]
"#,
    )
    .expect("write inter-session policy");

    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load inter-session policy");
    assert!(!settings.inter_session.allows(Path::new("/srv/public")));
    assert!(
        !settings
            .inter_session
            .allows(Path::new("/srv/private/repository"))
    );
}

/// Every supported unit maps to its exact checked number of seconds.
#[test]
fn retention_duration_accepts_exact_single_unit_grammar() {
    for (text, seconds) in [
        ("1s", 1),
        ("2m", 120),
        ("3h", 10_800),
        ("4d", 345_600),
        ("5w", 3_024_000),
    ] {
        let duration: RetentionDuration = text.parse().expect("valid retention duration");
        assert_eq!(duration.duration(), std::time::Duration::from_secs(seconds));
    }
}

/// Ambiguous, zero, signed, fractional, compound, whitespace, and overflowing
/// retention values fail instead of changing deletion policy.
#[test]
fn retention_duration_rejects_every_noncanonical_form() {
    for text in [
        "",
        "0s",
        "0d",
        "-1d",
        "+1d",
        "1.5d",
        "1d2h",
        " 1d",
        "1d ",
        "1",
        "1D",
        "18446744073709551616s",
        "18446744073709551615w",
    ] {
        assert!(
            text.parse::<RetentionDuration>().is_err(),
            "{text:?} must fail"
        );
    }
}

/// Retired day-count keys remain ordinary unknown fields and are never silently
/// translated into the new duration schema.
#[test]
fn retired_retention_keys_fail_as_unknown_fields() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "session_retention_days: 7\n",
    )
    .expect("write retired config");
    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("retired key must fail");
    let rendered = error.to_string();
    assert!(rendered.contains("session_retention_days"), "{rendered}");
    assert!(rendered.contains("unknown field"), "{rendered}");
}

/// Explicit null disables one leaf while omitted leaves continue inheriting
/// their built-in defaults.
#[test]
fn null_disables_one_retention_policy_and_omission_inherits() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "diagnostic_retention: null\n",
    )
    .expect("write null override");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load settings");
    assert_eq!(settings.session_retention(), None);
    assert_eq!(settings.agent_retention(), None);
    assert_eq!(settings.diagnostic_retention(), None);
}

/// File, drop-in, and CLI validation errors retain both their authored source
/// and exact retention key.
#[test]
fn invalid_retention_values_report_source_and_key_per_layer() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(td.path().join("harness.yaml"), "agent_retention: 0d\n").expect("base config");
    let base = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("base retention must fail")
        .to_string();
    assert!(
        base.contains("harness.yaml") && base.contains("agent_retention"),
        "{base}"
    );

    std::fs::write(td.path().join("harness.yaml"), "").expect("clear base");
    std::fs::create_dir(td.path().join("harness.d")).expect("drop-in dir");
    std::fs::write(
        td.path().join("harness.d/20-retention.yaml"),
        "diagnostic_retention: 1.5d\n",
    )
    .expect("drop-in config");
    let drop_in = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("drop-in retention must fail")
        .to_string();
    assert!(
        drop_in.contains("20-retention.yaml") && drop_in.contains("diagnostic_retention"),
        "{drop_in}"
    );

    std::fs::remove_file(td.path().join("harness.d/20-retention.yaml")).expect("remove drop-in");
    let overrides =
        [HarnessConfigCliOverride::from_str("session_retention=-1d").expect("override syntax")];
    let cli =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(td.path()), &[], &overrides)
            .expect_err("CLI retention must fail")
            .to_string();
    assert!(
        cli.contains("CLI override `session_retention`") && cli.contains("session_retention"),
        "{cli}"
    );
}

/// Ensures activating-input waits honor a one-minute default floor and retain
/// the established 1,440-minute ceiling unless users override both bounds.
#[test]
fn wait_timeout_bounds_default_and_override() {
    let built_in = HarnessSettings::built_in();
    let built_in_bounds = built_in.wait_timeout_bounds();
    assert_eq!(built_in_bounds.minimum().get(), 1);
    assert_eq!(built_in_bounds.maximum().get(), 1_440);
    assert_eq!(
        built_in_bounds.minimum_duration(),
        std::time::Duration::from_secs(60)
    );

    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "wait_timeout_minimum_minutes: 7\nwait_timeout_maximum_minutes: 11\n",
    )
    .expect("write harness config");

    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load overridden bounds");
    let bounds = settings.wait_timeout_bounds();
    assert_eq!(bounds.minimum().get(), 7);
    assert_eq!(bounds.maximum().get(), 11);
}

/// Ensures a higher-precedence one-field wait override retains the lower bound
/// from an earlier layer before the pair enters effective validation.
#[test]
fn wait_timeout_bounds_merge_partial_override_before_validation() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        "wait_timeout_minimum_minutes: 7\nwait_timeout_maximum_minutes: 11\n",
    )
    .expect("write base bounds");
    std::fs::create_dir_all(dir.join("harness.d")).expect("create drop-in directory");
    std::fs::write(
        dir.join("harness.d/10-maximum.yaml"),
        "wait_timeout_maximum_minutes: 13\n",
    )
    .expect("write partial override");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load layered bounds");
    let bounds = settings.wait_timeout_bounds();
    assert_eq!(bounds.minimum().get(), 7);
    assert_eq!(bounds.maximum().get(), 13);
}

/// The effective watch retry policy must preserve zero, the built-in threshold,
/// inclusive boundaries, and the full raw `u32` domain after config loading.
#[test]
fn agent_watch_retry_notification_threshold_defaults_and_overrides() {
    let disabled = AgentWatchRetryNotificationPolicy::from_raw(0);
    assert!(!disabled.suppresses(0));

    let built_in = HarnessSettings::built_in().agent_watch_retry_notification_threshold;
    assert!(built_in.suppresses(0));
    assert!(built_in.suppresses(5));
    assert!(!built_in.suppresses(6));

    let maximum = AgentWatchRetryNotificationPolicy::from_raw(u32::MAX);
    assert!(maximum.suppresses(u32::MAX));

    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "agent_watch_retry_notification_threshold: 0\n",
    )
    .expect("write config");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load threshold");
    assert!(
        !settings
            .agent_watch_retry_notification_threshold
            .suppresses(0)
    );
}

/// Notification delivery defaults preserve the four approved millisecond
/// classes and ordered delay invariant.
#[test]
fn notification_delivery_defaults_match_runtime_contract() {
    let policies = HarnessSettings::built_in().notification_delivery;
    assert_eq!(policies.user_prompt.idle(), Duration::ZERO);
    assert_eq!(policies.user_prompt.wait_any(), Duration::ZERO);
    assert_eq!(
        policies.user_prompt.wait_tool(),
        Duration::from_millis(5_000)
    );
    assert_eq!(policies.status.idle(), Duration::from_millis(120_000));
    assert_eq!(policies.status.wait_any(), Duration::from_millis(240_000));
    assert_eq!(policies.status.wait_tool(), Duration::from_millis(240_000));
    assert_eq!(policies.agent_message.idle(), Duration::ZERO);
    assert_eq!(
        policies.agent_message.wait_any(),
        Duration::from_millis(60_000)
    );
    assert_eq!(
        policies.agent_message.wait_tool(),
        Duration::from_millis(120_000)
    );
    assert_eq!(policies.external_message.idle(), Duration::ZERO);
    assert_eq!(policies.external_message.wait_any(), Duration::ZERO);
    assert_eq!(
        policies.external_message.wait_tool(),
        Duration::from_millis(30_000)
    );
}

/// Reordered notification delays fail configuration instead of being
/// normalized or saturated.
#[test]
fn notification_delivery_rejects_reordered_delays() {
    assert_eq!(
        NotificationDeliveryPolicy::from_millis(2, 1, 3).expect_err("reordered delays must fail"),
        "notification delivery delays must satisfy idle_ms <= wait_any_ms <= wait_tool_ms"
    );
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
    let bounds = settings.wait_timeout_bounds();
    assert_eq!(bounds.minimum().get(), 1);
    assert_eq!(bounds.maximum().get(), u64::from(u16::MAX));
}

/// Ensures the effective wait policy rejects unordered construction and
/// performs clamping through named semantic values rather than interchangeable
/// scalars.
#[test]
fn effective_wait_timeout_bounds_preserve_order_and_clamp_at_boundaries() {
    assert!(WaitTimeoutMinutes::new(0).is_none());
    let minimum = WaitTimeoutMinutes::new(7).expect("positive minimum");
    let maximum = WaitTimeoutMinutes::new(11).expect("positive maximum");
    assert!(WaitTimeoutBounds::new(maximum, minimum).is_none());

    let bounds = WaitTimeoutBounds::new(minimum, maximum).expect("ordered bounds");
    assert_eq!(bounds.clamp(1), minimum);
    assert_eq!(
        bounds.clamp(9).duration(),
        std::time::Duration::from_secs(9 * 60)
    );
    assert_eq!(bounds.clamp(u64::MAX), maximum);
}

/// Ensures raw wait validation retains its historical first-error ordering when
/// both authored bounds are invalid, before any effective policy can escape.
#[test]
fn wait_timeout_bounds_report_minimum_zero_before_maximum_zero() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "wait_timeout_minimum_minutes: 0\nwait_timeout_maximum_minutes: 0\n",
    )
    .expect("write invalid harness config");

    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("zero bounds must fail");
    assert!(
        error
            .to_string()
            .contains("wait_timeout_minimum_minutes must be at least 1")
    );
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
        r#"{ greeting: false, show_thinking: false, osc8_links: false, mouse: false, show_tools: "compact", show_messages: "self-summary", show_status: "minimal" }"#,
    )
    .expect("write");

    let s = load_cli_settings_in(&dirs_with_config(dir)).expect("load");
    assert!(!s.greeting);
    assert!(!s.show_thinking);
    assert!(!s.osc8_links);
    assert!(!s.mouse);
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

/// Mouse input remains enabled unless the user opts out in static CLI config.
#[test]
fn cli_settings_mouse_defaults_true() {
    assert!(CliSettings::built_in().mouse);
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

/// Rejects retired mechanism-oriented mouse names so only the public `mouse`
/// setting can configure the CLI behavior.
#[test]
fn cli_settings_reject_mouse_capture_alias() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("cli.yaml"), "mouse_capture: false\n").expect("write");

    let error = load_cli_settings_in(&dirs_with_config(dir)).expect_err("unknown key should fail");

    assert!(
        error.to_string().contains("mouse_capture"),
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
}

/// The shifted chat-editor default must coexist with ordinary Ctrl-O rather
/// than replacing its established prompt-editor action.
#[test]
fn built_in_bindings_distinguish_ctrl_o_and_ctrl_shift_o() {
    let bindings = super::default_cli_bindings();
    assert_eq!(
        bindings.get("C-o").map(|binding| binding.action.as_str()),
        Some("shell-prompt-edit")
    );
    assert_eq!(
        bindings.get("C-O").map(|binding| binding.action.as_str()),
        Some("shell-prompt-edit-chat")
    );
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
        show_internal_prompts: true,
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
    assert!(!loaded.show_internal_prompts);
    assert!(loaded.show_prompt_scroll_indicator);
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
        r#"{ show_diff: true, show_thinking: false, show_turn_stats: true, redraw_counter: true, redraw_history_size: 321, show_ui_io: true, show_tools: "compact", show_messages: "self-full", show_internal_prompts: true, notice_level: "warning", show_status: "minimal", show_prompt_scroll_indicator: false }"#,
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
            show_internal_prompts: true,
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
        show_internal_prompts: true,
        ..CliState::default()
    };

    let state = CliState::load_with_default(&dirs, default);

    assert!(!state.show_diff);
    assert_eq!(state.redraw_history_size, 321);
    assert!(state.show_internal_prompts);
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
                session_retention: "7d",
            }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
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
/// Ensures CLI config overrides parse as YAML and layer after config files.
#[test]
fn harness_config_cli_overrides_are_applied_last_and_typed() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"{
            session_retention: "7d",
            diagnostic_retention: "9d",
            extensions: {
                "core-shell": { config: { working_directory: "/from-file" } },
                "std-websearch": { enable: true },
            },
        }"#,
    )
    .expect("write");

    let file_settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load file layer");
    assert_eq!(
        file_settings.session_retention(),
        Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
    );
    assert_eq!(
        file_settings.diagnostic_retention(),
        Some(std::time::Duration::from_secs(9 * 24 * 60 * 60))
    );

    let overrides = [
        HarnessConfigCliOverride::from_str("session_retention=3d").expect("override"),
        HarnessConfigCliOverride::from_str("diagnostic_retention=null")
            .expect("diagnostic override"),
        HarnessConfigCliOverride::from_str(
            "extensions.core-shell.config.working_directory=/from-cli",
        )
        .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.std-websearch.enable=false")
            .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.core-shell.command=[\"tau\", \"ext\"]")
            .expect("override"),
        HarnessConfigCliOverride::from_str("extensions.core-shell.tool_prefix=work")
            .expect("override"),
    ];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(
        s.session_retention(),
        Some(std::time::Duration::from_secs(3 * 24 * 60 * 60))
    );
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

/// Ensures file parsing preserves explicit `require: false`, a generic CLI
/// override takes precedence, and without that override the other extension's
/// `require` field remains absent (`None`).
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

/// Ensures `extensions.<name>.require` rejects a string instead of silently
/// accepting or coercing it. The loader currently loses nested path context, so
/// this test does not make that diagnostic omission a contract.
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
        "agents.role_groups.engineer.roles.engineer.effort=0.25",
    )
    .expect("override")];

    let s = load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
        .expect("load");

    assert_eq!(
        s.roles["engineer"].effort,
        Some(tau_proto::NativeReasoningEffort::Low.into())
    );
}

/// Ensures malformed CLI config overrides fail explicitly at parse time.
#[test]
fn harness_config_cli_overrides_reject_bad_key_value() {
    assert!(HarnessConfigCliOverride::from_str("missing-equals").is_err());
    assert!(HarnessConfigCliOverride::from_str("=value").is_err());
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
                            disable_tool_tags: ["shell:*"],
                            enable_tool_tags: ["shell:cd"],
                            disable_tool_groups: ["shell"],
                            enable_tool_groups: ["search"],
                            disable_tools: ["grep"],
                            enable_tools: ["web_search"],
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
                  inference_compaction: disabled
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
                  inference_compaction: null
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
    assert_eq!(
        reviewer.inference_compaction,
        Some(RoleCompaction::ProviderDefault)
    );
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

/// Ensures required role skills are parsed from snake_case and camel case,
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
                  required_skills: [role-skill, shared-skill]
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
    assert_eq!(reviewer["compact-soon"].threshold.get(), 160_000);
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

/// Ensures a higher-precedence raw zero threshold wins layering but cannot
/// enter the effective alert domain, retaining the role-and-alert-specific
/// diagnostic.
#[test]
fn layered_zero_context_size_alert_threshold_cannot_escape_validation() {
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
          context_size_alerts:
            compact-soon:
              threshold: 100
"#,
    )
    .expect("write base config");
    std::fs::create_dir_all(dir.join("harness.d")).expect("create drop-in directory");
    std::fs::write(
        dir.join("harness.d/10-invalid.yaml"),
        r#"
agents:
  role_groups:
    custom:
      roles:
        reviewer:
          context_size_alerts:
            compact-soon:
              threshold: 0
"#,
    )
    .expect("write invalid override");

    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("layered zero threshold must fail");
    assert!(error.to_string().contains(
        "role `reviewer` context-size alert `compact-soon` requires a positive threshold"
    ));
}

/// Ensures disabling every role cannot let an incomplete agent-global alert
/// bypass role validation and escape through the public effective global map.
#[test]
fn disabled_roles_do_not_hide_invalid_global_context_size_alert_thresholds() {
    for alert_body in ["      enable: false\n", "      threshold: 0\n"] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(
            td.path().join("harness.yaml"),
            format!(
                r#"
agents:
  enable: false
  context_size_alerts:
    incomplete:
{alert_body}"#
            ),
        )
        .expect("write invalid global alert");

        let error = load_harness_settings_in(&dirs_with_config(td.path()))
            .expect_err("invalid global threshold must fail with no effective roles");
        assert!(
            error.to_string().contains(
                "agent-global context-size alert `incomplete` requires a positive threshold"
            ),
            "{error}"
        );
    }
}

/// Ensures a valid role override cannot mask an invalid global threshold that
/// remains independently exposed by effective harness settings.
#[test]
fn role_override_does_not_hide_invalid_global_context_size_alert_threshold() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  enable: false
  context_size_alerts:
    compact-soon:
      threshold: 0
  role_groups:
    custom:
      roles:
        reviewer:
          enable: true
          context_size_alerts:
            compact-soon:
              threshold: 100
"#,
    )
    .expect("write invalid global alert with valid role override");

    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("role override must not repair the separately exposed global alert");
    assert!(
        error.to_string().contains(
            "agent-global context-size alert `compact-soon` requires a positive threshold"
        ),
        "{error}"
    );
}

/// Ensures alert policy comparison is strict and remains explicitly
/// token-domain aware at the provider-usage boundary.
#[test]
fn context_size_alert_threshold_compares_provider_tokens_strictly() {
    let threshold = ContextSizeAlertThreshold::new(100).expect("positive threshold");
    assert!(!threshold.is_exceeded_by(tau_proto::TokenCount::new(100)));
    assert!(threshold.is_exceeded_by(tau_proto::TokenCount::new(101)));
}

/// Ensures neither direct semantic construction nor direct effective-alert
/// deserialization can create a zero context-size alert threshold.
#[test]
fn context_size_alert_threshold_rejects_zero_at_public_boundaries() {
    assert!(ContextSizeAlertThreshold::new(0).is_none());
    let error = serde_yaml_ng::from_str::<ContextSizeAlert>("threshold: 0\n")
        .expect_err("direct effective alert must reject zero");
    assert!(
        error
            .to_string()
            .contains("context-size alert threshold must be positive")
    );
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
                        reviewer: { effort: 0.75 },
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
                        engineer: { effort: 0.75 },
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
        ("default_role", r#"{ default_role: stale }"#),
        (
            "role_groups",
            r#"{ role_groups: { stale: { roles: { stale: {} } } } }"#,
        ),
        (
            "role_groups",
            r#"{ role_groups: { stale: { roles: { stale: {} } } } }"#,
        ),
        (
            "prompt_fragments",
            r#"{ prompt_fragments: [{ name: stale, priority: 1, text: stale }] }"#,
        ),
        (
            "prompt_fragments",
            r#"{ prompt_fragments: [{ name: stale, priority: 1, text: stale }] }"#,
        ),
        ("required_skills", r#"{ required_skills: [stale] }"#),
        ("required_skills", r#"{ required_skills: [stale] }"#),
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
        "default_role=stale",
        "role_groups={stale: {roles: {stale: {}}}}",
        "role_groups={stale: {roles: {stale: {}}}}",
        "prompt_fragments=[{ name: stale, priority: 1, text: stale }]",
        "prompt_fragments=[{ name: stale, priority: 1, text: stale }]",
        "required_skills=[stale]",
        "required_skills=[stale]",
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
    // harness reports an explicit startup error for this empty effective role
    // set.
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
    // CLI role typos must fail startup instead of silently leaving the
    // effective role set unchanged.
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
            session_retention: "7d",
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
            session_retention: "14d",
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
    assert_eq!(
        s.session_retention(),
        Some(std::time::Duration::from_secs(14 * 24 * 60 * 60))
    );
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
          effort: 0.5
          verbosity: high
          thinking_summary: concise
          service_tier: flex
          inference_compaction: { threshold: 123456 }
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
    assert_eq!(
        inherited.effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
    assert_eq!(inherited.verbosity, Some(tau_proto::Verbosity::High));
    assert_eq!(
        inherited.thinking_summary,
        Some(tau_proto::ThinkingSummary::Concise)
    );
    assert_eq!(inherited.service_tier, Some(tau_proto::ServiceTier::Flex));
    assert_eq!(
        inherited.inference_compaction,
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
          effort: 0.1
          verbosity: high
          thinking_summary: detailed
          service_tier: fast
          role_groups:
            custom:
              model: openai/base-group
              effort: 0.25
              roles:
                reviewer:
                  model: openai/base-role
                  effort: 0.75
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
          effort: disabled
          thinking_summary: null
          service_tier: flex
          role_groups:
            custom:
              effort: 0.5
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
    assert_eq!(
        reviewer.effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
    );
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
          effort: increase:0.25
          verbosity: decrease:99
          thinking_summary: decrease
          role_groups:
            custom:
              effort: increase:99.0
              thinking_summary: increase:2
              roles:
                reviewer:
                  effort: decrease:99.0
                  verbosity: increase
                  thinking_summary: increase:99
        "#,
    )
    .expect("write harness");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    let reviewer = &settings.roles["reviewer"];
    assert_eq!(
        reviewer.effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
    );
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
          effort: decrease:0.25
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
      effort: 0.75
      roles:
        precedence-senior:
          model: chatgpt/gpt-5.6-sol
          effort: 0.5
"#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        settings.roles["precedence-junior"].effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
    assert_eq!(
        settings.roles["precedence"].effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
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
        Some(tau_proto::NativeReasoningEffort::Medium.into())
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
    let default = profile_selection("default");
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

/// Ensures a configured default normalizes surrounding ASCII spaces/tabs while
/// remaining one transport-safe profile name for daemon forwarding.
#[test]
fn default_profile_normalizes_one_name_for_daemon_round_trip() {
    for value in [" focused ", "\tfocused\t"] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(
            td.path().join("harness.yaml"),
            format!("default_profile: {value:?}\n"),
        )
        .expect("write default selection");
        let profile = default_profile_in(&dirs_with_config(td.path()))
            .expect("read default profile")
            .expect("configured default profile");
        assert_eq!(profile.as_str(), "focused");
        assert_eq!(
            ProfileSelection::parse(profile.to_string()).expect("reparse daemon environment"),
            ProfileSelection::parse("focused").expect("expected selection")
        );
    }
    for value in [" \t ", "\n", "\u{a0}", "focused,review"] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(
            td.path().join("harness.yaml"),
            format!("default_profile: {value:?}\n"),
        )
        .expect("write invalid default");
        assert!(
            default_profile_in(&dirs_with_config(td.path())).is_err(),
            "{value:?} must not select a fallback"
        );
    }
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
    let focused = profile_selection("focused");
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

    let profile = profile_selection("focused");
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
          effort: decrease:0.25
profiles:
  focused:
    agents:
      role_groups:
        precedence:
          effort: 0.75
"#,
    )
    .expect("write profile");
    let profile = profile_selection("focused");
    let overrides =
        [
            HarnessConfigCliOverride::from_str(
                "agents.role_groups.precedence.effort=increase:0.25",
            )
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
        Some(tau_proto::NativeReasoningEffort::High.into())
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
  effort: 0.25
  role_groups:
    base:
      roles:
        base-role: {}
profiles:
  focused:
    agents:
      effort: increase:0.25
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

    let profile = profile_selection("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[RoleCliOverride::Enable("profile-role".to_owned())],
        &overrides,
    )
    .expect("load selected profile");

    assert_eq!(
        settings.roles["base-role"].effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
    assert_eq!(
        settings.roles["profile-role"].effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
    assert_eq!(
        settings.roles["profile-role"].verbosity,
        Some(tau_proto::Verbosity::High)
    );
    assert_eq!(settings.extensions["core-shell"].enable, Some(true));
}

/// Ensures profile-wide Tau-state access layers over base and drop-in values,
/// while the later command-line harness layer remains authoritative.
#[test]
fn selected_profile_layers_tau_state_access_before_cli_overrides() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
tau_state_access: hidden
profiles:
  focused:
    tau_state_access: read_only
"#,
    )
    .expect("write base profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-focused.yaml"),
        r#"
profiles:
  focused:
    tau_state_access: hidden
"#,
    )
    .expect("write profile drop-in");

    let profile = profile_selection("focused");
    let profiled = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(profiled.tau_state_access, TauStateAccess::Hidden);

    let overrides = [
        HarnessConfigCliOverride::from_str("tau_state_access=read_only")
            .expect("Tau-state access override"),
    ];
    let overridden = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &overrides,
    )
    .expect("load selected profile with command-line override");
    assert_eq!(overridden.tau_state_access, TauStateAccess::ReadOnly);
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

    let profile = profile_selection("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert!(settings.roles.contains_key("profile-role"));
    assert_eq!(settings.extensions["core-shell"].enable, Some(false));

    let profile = profile_selection("missing");
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
  effort: 0.25
  role_groups:
    profile:
      roles:
        profile-role: {}
profiles:
  focused:
    agents:
      effort: increase:0.25
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
      effort: increase:0.25
    extensions:
      core-shell:
        enable: true
"#,
    )
    .expect("write profile drop-in");

    let profile = profile_selection("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(
        settings.roles["profile-role"].effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
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

    let profile = profile_selection("focused");
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
      default_role: first-profile-role
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

    let profile = profile_selection("focused");
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

    let profile = profile_selection("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load selected profile");

    assert_eq!(settings.default_role, None);
}

/// Ensures profiles reject harness and extension process settings outside their
/// explicit role, extension-enable, and extension-config surface.
#[test]
fn selected_profile_rejects_unsupported_settings_and_extensions() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
profiles:
  invalid:
    session_retention: 1d
"#,
    )
    .expect("write profiles");

    let profile = profile_selection("invalid");
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

/// Ensures arbitrary extension config participates in base, selected-profile,
/// drop-in, and ordered CLI precedence without replacing unrelated nested keys.
#[test]
fn selected_profile_recursively_merges_extension_config_before_cli() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
extensions:
  core-shell:
    config:
      shell:
        command: sh
        nullable: base
        extra_env:
          BASE: base
profiles:
  focused:
    extensions:
      core-shell:
        config:
          shell:
            allowlist:
              - workdir: /base/**
                command: cargo *
"#,
    )
    .expect("write base profile");
    std::fs::create_dir(td.path().join("harness.d")).expect("create drop-ins");
    std::fs::write(
        td.path().join("harness.d/20-focused.yaml"),
        r#"
profiles:
  focused:
    extensions:
      core-shell:
        config:
          shell:
            nullable: null
            extra_env:
              PROFILE: selected
"#,
    )
    .expect("write profile drop-in");
    let profile = profile_selection("focused");
    let overrides =
        [
            HarnessConfigCliOverride::from_str("extensions.core-shell.config.shell.command=bash")
                .expect("CLI override"),
        ];

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &overrides,
    )
    .expect("load selected profile");
    assert_eq!(
        settings.extensions["core-shell"].config,
        Some(serde_json::json!({
            "shell": {
                "command": "bash",
                "nullable": null,
                "extra_env": {
                    "BASE": "base",
                    "PROFILE": "selected"
                },
                "allowlist": [{
                    "workdir": "/base/**",
                    "command": "cargo *"
                }]
            }
        }))
    );
}

/// Ensures a top-level null profile extension config preserves the historical
/// absent/no-op behavior rather than deleting the base config value.
#[test]
fn selected_profile_top_level_null_extension_config_is_no_op() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
extensions:
  core-shell:
    config:
      marker: base
profiles:
  focused:
    extensions:
      core-shell:
        config: null
"#,
    )
    .expect("write profile");
    let profile = profile_selection("focused");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile),
        &[],
        &[],
    )
    .expect("load profile");

    assert_eq!(
        settings.extensions["core-shell"].config,
        Some(serde_json::json!({"marker": "base"}))
    );
}

/// Ensures one-shot harness config accepts relative provider defaults, starts
/// otherwise-unset values from neutral bases, and rejects non-positive
/// directional magnitudes before they can reverse or erase the named direction.
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
        HarnessConfigCliOverride::from_str("agents.effort=increase:0.25").expect("effort"),
        HarnessConfigCliOverride::from_str("agents.verbosity=decrease:99").expect("verbosity"),
        HarnessConfigCliOverride::from_str("agents.thinking_summary=increase:99")
            .expect("thinking summary"),
    ];

    let settings =
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &overrides)
            .expect("load");
    let inherited = &settings.roles["inherited"];
    assert_eq!(
        inherited.effort,
        Some(tau_proto::NativeReasoningEffort::High.into())
    );
    assert_eq!(inherited.verbosity, Some(tau_proto::Verbosity::Low));
    assert_eq!(
        inherited.thinking_summary,
        Some(tau_proto::ThinkingSummary::Detailed)
    );

    let zero = [HarnessConfigCliOverride::from_str("agents.effort=increase:0").expect("parse")];
    assert!(
        load_harness_settings_with_cli_overrides_in(&dirs_with_config(dir), &[], &zero).is_err()
    );
    for value in ["increase:-0.25", "decrease:-0.25", "decrease:0"] {
        let override_value =
            [
                HarnessConfigCliOverride::from_str(&format!("agents.effort={value}"))
                    .expect("parse"),
            ];
        assert!(
            load_harness_settings_with_cli_overrides_in(
                &dirs_with_config(dir),
                &[],
                &override_value
            )
            .is_err(),
            "{value} must be rejected"
        );
    }
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
        HarnessConfigCliOverride::from_str("agents.thinking_summary=detailed")
            .expect("thinking summary"),
        HarnessConfigCliOverride::from_str("agents.service_tier=fast").expect("service tier"),
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
            "agents.prompt_fragments=[{ name: \"global.cli\", priority: 64, text: \"Follow the run policy.\" }]",
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
                        custom: { description: "Custom local role", effort: 0.5, disable_tools: ["shell"] },
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
    assert_eq!(
        s.roles["custom"].effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
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
                    effort: 0.25,
                    tools: ["read"],
                    enable_tools: ["grep"],
                    prompt_fragments: [
                        { name: "review.shared", priority: 80, text: "Review carefully." },
                    ],
                    roles: {
                        quick: {},
                        deep: {
                            effort: 0.9,
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
    assert_eq!(
        quick.effort,
        Some(tau_proto::NativeReasoningEffort::Low.into())
    );
    assert_eq!(quick.tools, Some(vec![tau_proto::ToolName::new("read")]));
    assert_eq!(quick.enable_tools, vec![tau_proto::ToolName::new("grep")]);
    assert!(
        quick
            .prompt_fragments
            .iter()
            .any(|fragment| fragment.name == "review.shared")
    );

    let deep = &s.roles["deep"];
    assert_eq!(
        deep.effort,
        Some(tau_proto::NativeReasoningEffort::XHigh.into())
    );
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

/// File-backed prompt fragments resolve relative to the Tau config directory
/// and keep the same resolved-content de-duplication as inline fragments.
#[test]
fn harness_prompt_fragment_text_files_resolve_and_deduplicate() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("shared.hbs"), "Use {{role.name}} carefully.").expect("write fragment");
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
agents:
  prompt_fragments:
    - { name: shared.policy, priority: 65, textFile: shared.hbs }
  role_groups:
    custom:
      roles:
        custom:
          prompt_fragments:
            - { name: custom.file, priority: 70, textFile: shared.hbs }
"#,
    )
    .expect("write harness");
    std::fs::create_dir(dir.join("harness.d")).expect("create drop-in directory");
    std::fs::write(
        dir.join("harness.d/10-inline.yaml"),
        r#"
agents:
  prompt_fragments:
    - { name: shared.policy, priority: 65, text: "Use {{role.name}} carefully." }
"#,
    )
    .expect("write drop-in");

    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load config");
    assert_eq!(
        settings
            .prompt_fragments
            .iter()
            .filter(|fragment| fragment.name == "shared.policy")
            .count(),
        1
    );
    assert_eq!(
        settings.roles["custom"]
            .prompt_fragments
            .iter()
            .find(|fragment| fragment.name == "custom.file")
            .map(|fragment| fragment.text.as_str()),
        Some("Use {{role.name}} carefully.")
    );
}

/// Absolute file-backed prompt fragments do not require a configured Tau
/// directory, including when supplied through a one-shot CLI config layer.
#[test]
fn harness_prompt_fragment_absolute_text_file_works_without_config_dir() {
    let td = TempDir::new().expect("tempdir");
    let fragment_path = td.path().join("absolute.hbs");
    std::fs::write(&fragment_path, "Absolute fragment.").expect("write fragment");
    let override_ = HarnessConfigCliOverride::from_str(&format!(
        "agents.prompt_fragments=[{{name: absolute, priority: 66, textFile: {:?}}}]",
        fragment_path.display().to_string()
    ))
    .expect("parse override");

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &TauDirs {
            config_dir: None,
            state_dir: None,
        },
        None,
        &[],
        &[override_],
    )
    .expect("load absolute fragment");
    assert!(settings.prompt_fragments.iter().any(|fragment| {
        fragment.name == "absolute" && fragment.text.as_str() == "Absolute fragment."
    }));
}

/// Conflicting prompt text sources and unreadable files fail configuration
/// without echoing inline prompt contents into the diagnostic.
#[test]
fn harness_prompt_fragment_text_file_errors_are_fatal_and_content_safe() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
agents:
  role_groups:
    disabled:
      enable: false
      prompt_fragments:
        - name: disabled.private
          priority: 70
          textFile: missing-private.hbs
      roles:
        disabled: {}
"#,
    )
    .expect("write unreadable fixture");
    let error = load_harness_settings_in(&dirs_with_config(dir))
        .expect_err("disabled role file must still be read");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("disabled.private") || diagnostic.contains("textFile"));
    assert!(diagnostic.contains("missing-private.hbs"));

    std::fs::write(
        dir.join("harness.yaml"),
        r#"
agents:
  prompt_fragments:
    - name: conflicting.private
      priority: 70
      text: "MILDLY PRIVATE INLINE CONTENT"
      textFile: private.hbs
"#,
    )
    .expect("write conflict fixture");
    let error =
        load_harness_settings_in(&dirs_with_config(dir)).expect_err("conflict must be rejected");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("mutually exclusive"));
    assert!(!diagnostic.contains("MILDLY PRIVATE INLINE CONTENT"));
}

/// Only selected profile prompt files are loaded; dormant profiles must not
/// make an otherwise valid harness fail because their local files are absent.
#[test]
fn harness_prompt_fragment_text_files_only_load_for_selected_profiles() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
profiles:
  dormant:
    agents:
      prompt_fragments:
        - { name: dormant.private, priority: 70, textFile: absent.hbs }
"#,
    )
    .expect("write profile fixture");

    load_harness_settings_in(&dirs_with_config(dir)).expect("ignore dormant profile file");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(dir),
        Some(&profile_selection("dormant")),
        &[],
        &[],
    )
    .expect_err("selected profile file must be read");
    assert!(error.to_string().contains("absent.hbs"));
}

/// Selected profiles resolve file-backed fragments before replaying every
/// profile source layer, preserving additive and full-equality de-duplication.
#[test]
fn selected_profile_prompt_fragment_text_files_replay_across_layers() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(dir.join("shared.hbs"), "Selected shared fragment.").expect("write shared");
    std::fs::write(dir.join("extra.hbs"), "Selected extra fragment.").expect("write extra");
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
profiles:
  selected:
    agents:
      prompt_fragments:
        - { name: selected.shared, priority: 70, textFile: shared.hbs }
"#,
    )
    .expect("write base profile");
    std::fs::create_dir(dir.join("harness.d")).expect("create drop-in directory");
    std::fs::write(
        dir.join("harness.d/10-selected.yaml"),
        r#"
profiles:
  selected:
    agents:
      prompt_fragments:
        - { name: selected.shared, priority: 70, text: "Selected shared fragment." }
        - { name: selected.extra, priority: 71, textFile: extra.hbs }
"#,
    )
    .expect("write profile drop-in");

    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(dir),
        Some(&profile_selection("selected")),
        &[],
        &[],
    )
    .expect("load selected profile");
    assert_eq!(
        settings
            .prompt_fragments
            .iter()
            .filter(|fragment| fragment.name == "selected.shared")
            .count(),
        1
    );
    assert!(settings.prompt_fragments.iter().any(|fragment| {
        fragment.name == "selected.extra" && fragment.text.as_str() == "Selected extra fragment."
    }));
}

/// Dormant profiles remain schema-validated even though Tau does not read their
/// file paths, preventing invalid source shapes from hiding until selection.
#[test]
fn unselected_profile_prompt_fragment_text_files_are_schema_validated_without_reads() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    for (fragment, expected) in [
        (
            r#"{ name: invalid.type, priority: 70, textFile: [must-not-read] }"#,
            "must be a path string",
        ),
        (
            r#"{ name: invalid.conflict, priority: 70, text: "PRIVATE CONTENT", textFile: must-not-read.hbs }"#,
            "mutually exclusive",
        ),
    ] {
        std::fs::write(
            dir.join("harness.yaml"),
            format!(
                "profiles:\n  dormant:\n    agents:\n      prompt_fragments:\n        - {fragment}\n"
            ),
        )
        .expect("write dormant profile");
        let error = load_harness_settings_in(&dirs_with_config(dir))
            .expect_err("invalid dormant profile source must fail");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains(expected), "{diagnostic}");
        assert!(!diagnostic.contains("PRIVATE CONTENT"));
        assert!(!diagnostic.contains("failed to read"), "{diagnostic}");
    }
}

/// Ensures the embedded built-in role catalog contains only engineer roles,
/// gives each role the capability-gated delegate-role fragment, and resolves
/// its relative effort presets from the shared default.
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
    assert_eq!(
        engineer_junior
            .effort
            .expect("built-in junior effort")
            .to_string(),
        "0.25"
    );
    let engineer = &s.roles["engineer"];
    assert_eq!(
        engineer
            .effort
            .expect("built-in engineer effort")
            .to_string(),
        "0.5"
    );
    let delegate_roles = engineer
        .prompt_fragments
        .iter()
        .find(|fragment| fragment.name == "agent.available-roles")
        .expect("global delegate-role prompt fragment");
    assert_eq!(delegate_roles.priority, PromptPriority::new(800));
    assert!(
        delegate_roles
            .text
            .contains("{{#if (tool_available capabilities.tools \"agent_start\")~}}")
    );
    assert!(
        delegate_roles
            .text
            .contains("## Available agent roles for `agent_start`")
    );
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
    assert_eq!(
        engineer_senior
            .effort
            .expect("built-in senior effort")
            .to_string(),
        "0.75"
    );
}

/// User role layers override a built-in relative effort preset instead of
/// being re-adjusted after the higher-precedence value is applied.
#[test]
fn harness_user_role_effort_overrides_built_in_relative_preset() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  role_groups:
    engineer:
      roles:
        engineer-junior:
          effort: 0.8
"#,
    )
    .expect("write config");

    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load config");
    assert_eq!(
        settings.roles["engineer-junior"]
            .effort
            .expect("user override effort")
            .to_string(),
        "0.8"
    );
}

/// A user agent-wide effort rebases the built-in relative junior and senior
/// presets while engineer inherits the new shared value.
#[test]
fn harness_user_agent_effort_rebases_built_in_relative_presets() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(td.path().join("harness.yaml"), "agents:\n  effort: 0.6\n")
        .expect("write config");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load config");
    for (role, effort) in [
        ("engineer-junior", "0.35"),
        ("engineer", "0.6"),
        ("engineer-senior", "0.85"),
    ] {
        assert_eq!(
            settings.roles[role]
                .effort
                .expect("rebased built-in effort")
                .to_string(),
            effort
        );
    }
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
                        custom: { effort: 0.5, tools: ["read"] },
                    },
                },
            },
            },
        }"#,
    )
    .expect("write");

    let s = load_harness_settings_in(&dirs_with_config(dir)).expect("load");
    assert_eq!(
        s.roles["custom"].effort,
        Some(tau_proto::NativeReasoningEffort::Medium.into())
    );
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

/// The nested receiver merges field-by-field across layers and defaults
/// auto-start on.
#[test]
fn inter_session_receiver_merges_and_defaults_auto_start() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
inter_session:
  receiver:
    role: engineer
"#,
    )
    .expect("write base");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("load receiver policy");
    let receiver = settings.inter_session.receiver.expect("receiver");
    assert_eq!(receiver.role, "engineer");
    assert!(receiver.auto_start);

    std::fs::create_dir(td.path().join("harness.d")).expect("drop-in dir");
    std::fs::write(
        td.path().join("harness.d/10-auto-start.yaml"),
        "inter_session: { receiver: { auto_start: false } }",
    )
    .expect("write override");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("merge receiver fields");
    let receiver = settings.inter_session.receiver.expect("receiver");
    assert_eq!(receiver.role, "engineer");
    assert!(!receiver.auto_start);
}

/// Explicit null disables bare-session receiver addressing.
#[test]
fn inter_session_receiver_null_disables_receiver() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "inter_session: { receiver: { role: engineer } }",
    )
    .expect("write base");
    std::fs::create_dir(td.path().join("harness.d")).expect("drop-in dir");
    std::fs::write(
        td.path().join("harness.d/10-disable.yaml"),
        "inter_session: { receiver: null }",
    )
    .expect("write disable");
    let settings =
        load_harness_settings_in(&dirs_with_config(td.path())).expect("disable receiver");
    assert_eq!(settings.inter_session.receiver, None);
}

/// Receiver role validation uses the final enabled role set after layered role
/// and profile processing.
#[test]
fn inter_session_receiver_requires_enabled_effective_role() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
inter_session:
  receiver:
    role: project-manager
default_profile: no-manager
agents:
  role_groups:
    manager:
      roles:
        project-manager: {}
profiles:
  no-manager:
    agents:
      role_groups:
        manager:
          roles:
            project-manager:
              enable: false
"#,
    )
    .expect("write");
    let error =
        load_harness_settings_in(&dirs_with_config(td.path())).expect_err("disabled receiver role");
    assert!(
        error
            .to_string()
            .contains("inter-session receiver role `project-manager` is not enabled")
    );
}

/// Removed multi-role and superseded flat schemas fail explicitly.
#[test]
fn inter_session_configuration_rejects_removed_receiver_schemas() {
    for yaml in [
        "agents: { role_groups: { manager: { peer_entrypoint: {} } } }",
        "agents: { role_groups: { manager: { peerEntryPoint: { autoStartRole: project-manager } } } }",
        "agents: { role_groups: { manager: { inter_session_receiver: true } } }",
        "agents: { role_groups: { manager: { roles: { project-manager: { inter_session_auto_start: true } } } } }",
        "inter_session: { receiver_role: coordinator }",
        "inter_session: { auto_start: true }",
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
/// Ensures missing optional user config files fall back to the built-in layers.
#[test]
fn missing_user_files_load_the_built_in_baseline() {
    let td = TempDir::new().expect("tempdir");
    let _cli = load_cli_settings_in(&dirs_with_config(td.path())).expect("cli");
    let harness = load_harness_settings_in(&dirs_with_config(td.path())).expect("harness");
    assert!(
        harness
            .default_role
            .as_ref()
            .is_some_and(|role| harness.roles.contains_key(role))
    );
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

    let profile = profile_selection("focused");
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

    let profile = profile_selection("focused");
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
        r#"{ agents: { role_groups: { engineer: { roles: { "engineer-senior": { enable: true, effort: 0.9 } } } } } }"#,
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
    let cli_sample = include_str!("../../../../config/cli.yaml");
    assert!(
        cli_sample.contains("mouse: false"),
        "sample must show the static mouse opt-out"
    );

    std::fs::write(dir.join("cli.yaml"), cli_sample).expect("write cli");
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
        "extensions:\n  work:\n    command: [demo]\n    tool_prefix: team_ops\n",
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

/// Ensures an omitted field uses the public read-only default while an explicit
/// hidden instance policy remains distinguishable for later precedence
/// handling.
#[test]
fn tau_state_access_omission_defaults_to_read_only_and_preserves_explicit_hidden() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "extensions:\n  inherited:\n    command: [demo]\n  hidden:\n    command: [demo]\n    tau_state_access: hidden\n",
    )
    .expect("write");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    assert_eq!(settings.tau_state_access, TauStateAccess::ReadOnly);
    assert_eq!(settings.extensions["inherited"].tau_state_access, None);
    assert_eq!(
        settings.extensions["hidden"].tau_state_access,
        Some(TauStateAccess::Hidden)
    );
}

/// Ensures stale explicit `legacy` values fail at every Tau-state configuration
/// layer instead of silently falling back to a supported policy.
#[test]
fn tau_state_access_rejects_removed_legacy_configuration() {
    for yaml in [
        "tau_state_access: legacy\n",
        "extensions:\n  shell:\n    command: [demo]\n    tau_state_access: legacy\n",
    ] {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(td.path().join("harness.yaml"), yaml).expect("write stale configuration");
        let error = load_harness_settings_in(&dirs_with_config(td.path()))
            .expect_err("legacy configuration must be rejected");
        assert!(
            error.to_string().contains("legacy"),
            "error should identify the rejected value: {error}"
        );
    }

    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "profiles:\n  stale:\n    tau_state_access: legacy\n",
    )
    .expect("write stale profile");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&profile_selection("stale")),
        &[],
        &[],
    )
    .expect_err("selected legacy profile must be rejected");
    assert!(
        error.to_string().contains("legacy"),
        "profile error should identify the rejected value: {error}"
    );

    let cli_td = TempDir::new().expect("CLI tempdir");
    let override_value = HarnessConfigCliOverride::from_str("tau_state_access=legacy")
        .expect("generic override syntax");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(cli_td.path()),
        None,
        &[],
        &[override_value],
    )
    .expect_err("legacy command-line override must be rejected");
    assert!(
        error.to_string().contains("legacy"),
        "override error should identify the rejected value: {error}"
    );
}

/// Ensures runtime socket access stays fail-closed unless one component
/// explicitly requests the ambient view.
#[test]
fn tau_runtime_socket_access_requires_an_explicit_component_opt_out() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "extensions:\n  masked:\n    command: [demo]\n  trusted:\n    command: [demo]\n    tau_runtime_socket_access: ambient\n",
    )
    .expect("write");
    let settings = load_harness_settings_in(&dirs_with_config(td.path())).expect("load");
    assert_eq!(
        settings.extensions["masked"].tau_runtime_socket_access,
        None
    );
    assert_eq!(
        settings.extensions["trusted"].tau_runtime_socket_access,
        Some(TauRuntimeSocketAccess::Ambient)
    );
}

/// Ensures the removed legacy runtime-socket value cannot silently restore
/// ambient socket access through file or command-line configuration.
#[test]
fn tau_runtime_socket_access_rejects_removed_legacy_configuration() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "extensions:\n  trusted:\n    command: [demo]\n    tau_runtime_socket_access: legacy\n",
    )
    .expect("write stale configuration");
    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("legacy runtime socket configuration must be rejected");
    assert!(
        error.to_string().contains("legacy"),
        "error should identify the rejected value: {error}"
    );

    let cli_td = TempDir::new().expect("CLI tempdir");
    let override_value =
        HarnessConfigCliOverride::from_str("extensions.trusted.tau_runtime_socket_access=legacy")
            .expect("generic override syntax");
    let error = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(cli_td.path()),
        None,
        &[],
        &[override_value],
    )
    .expect_err("legacy runtime socket command-line override must be rejected");
    assert!(
        error.to_string().contains("legacy"),
        "override error should identify the rejected value: {error}"
    );
}

/// Named compaction policies must preserve their lifecycle and status selector
/// instead of lowering it into the singular provider threshold.
#[test]
fn agent_role_deserializes_named_and_inference_compaction_independently() {
    let role: AgentRole = serde_yaml_ng::from_str(
        r#"
inference_compaction: disabled
compactions:
  eager:
    threshold: 160000
    when:
      at: outer_turn_finished
      statuses: [done, blocked]
  fallback:
    threshold: provider_default
"#,
    )
    .expect("valid role");
    assert_eq!(role.inference_compaction, Some(RoleCompaction::Disabled));
    assert_eq!(role.compactions.len(), 2);
    assert_eq!(
        role.compactions["eager"].when,
        ContextPolicyWhen {
            at: ContextPolicyPoint::OuterTurnFinished,
            statuses: Some(vec![
                tau_proto::AgentWorkStatusPhase::Done,
                tau_proto::AgentWorkStatusPhase::Blocked,
            ]),
        }
    );
    assert_eq!(
        role.compactions["fallback"].when,
        ContextPolicyWhen::default()
    );
}
/// Reserve-based role policy input must retain its distinct boundary form
/// through the public serde surface.
#[test]
fn role_compaction_reserve_round_trips() {
    let policy: RoleCompaction =
        serde_yaml_ng::from_str("reserve: 25000\n").expect("reserve policy");
    assert_eq!(policy, RoleCompaction::Reserve(25_000));
    assert_eq!(
        serde_yaml_ng::to_string(&policy).expect("serialize reserve"),
        "reserve: 25000\n"
    );
}

/// The singular and legacy form shares the same exclusive boundary contract as
/// named policies.
#[test]
fn role_compaction_rejects_threshold_and_reserve_together() {
    let error = serde_yaml_ng::from_str::<RoleCompaction>("threshold: 75000\nreserve: 25000\n")
        .expect_err("ambiguous boundary must fail");
    assert!(
        error
            .to_string()
            .contains("cannot set both `threshold` and `reserve`")
    );
}

/// Named policies must reject ambiguous same-layer boundaries with a clear
/// diagnostic instead of choosing one by field order.
#[test]
fn named_compaction_rejects_threshold_and_reserve_together() {
    let error = serde_yaml_ng::from_str::<CompactionPolicy>("threshold: 75000\nreserve: 25000\n")
        .expect_err("ambiguous boundary must fail");
    assert!(
        error
            .to_string()
            .contains("cannot set both `threshold` and `reserve`")
    );
}

/// Reserve zero and equality with the context window are exact arithmetic
/// boundaries; only a reserve larger than the window underflows.
#[test]
fn compaction_reserve_resolution_preserves_exact_edges() {
    assert_eq!(
        compaction_threshold_from_reserve(100_000, 0).expect("zero reserve"),
        100_000
    );
    assert_eq!(
        compaction_threshold_from_reserve(100_000, 100_000).expect("equal reserve"),
        0
    );
    assert_eq!(
        compaction_threshold_from_reserve(100_000, 100_001),
        Err(CompactionReserveError {
            context_window: 100_000,
            reserve: 100_001,
        })
    );
}

/// A later config layer may switch one named policy from an absolute threshold
/// to a reserve without inheriting both mutually exclusive keys.
#[test]
fn named_compaction_layer_can_replace_threshold_with_reserve() {
    let mut policy = CompactionPolicy {
        threshold: CompactionPolicyThreshold::Tokens(75_000),
        enable: true,
        when: ContextPolicyWhen::default(),
    };
    let patch: CompactionPolicyPatch =
        serde_yaml_ng::from_str("reserve: 25000\n").expect("reserve patch");
    patch.apply_to(&mut policy);
    assert_eq!(policy.threshold, CompactionPolicyThreshold::Reserve(25_000));
    let yaml = serde_yaml_ng::to_string(&policy).expect("serialize policy");
    assert!(yaml.contains("reserve: 25000"));
    assert!(!yaml.contains("threshold:"));
}

/// The public boundary enum must not erase reserve semantics when callers
/// serialize it outside an enclosing named policy.
#[test]
fn compaction_policy_threshold_reserve_round_trips_directly() {
    let boundary = CompactionPolicyThreshold::Reserve(25_000);
    let yaml = serde_yaml_ng::to_string(&boundary).expect("serialize reserve boundary");
    assert_eq!(yaml, "reserve: 25000\n");
    assert_eq!(
        serde_yaml_ng::from_str::<CompactionPolicyThreshold>(&yaml)
            .expect("deserialize reserve boundary"),
        boundary
    );
}

/// Named policies expose reserve only as a sibling boundary key; nesting it
/// under `threshold` would create an undocumented second grammar.
#[test]
fn named_compaction_rejects_nested_reserve_threshold() {
    let error = serde_yaml_ng::from_str::<CompactionPolicy>("threshold: { reserve: 25000 }\n")
        .expect_err("nested reserve must fail");
    assert!(error.to_string().contains("did not match any variant"));
}

/// Direct reserve boundary maps remain closed so misspelled fields cannot be
/// ignored while preserving an apparently valid reserve.
#[test]
fn compaction_policy_threshold_reserve_rejects_unknown_fields() {
    let error =
        serde_yaml_ng::from_str::<CompactionPolicyThreshold>("reserve: 25000\nunexpected: true\n")
            .expect_err("unknown reserve field must fail");
    assert!(error.to_string().contains("did not match any variant"));
}

/// An empty status set is almost certainly a configuration error; users must
/// use null or omission to express an unrestricted policy.
#[test]
fn context_policy_rejects_empty_status_set() {
    let error =
        serde_yaml_ng::from_str::<ContextPolicyWhen>("at: before_inference\nstatuses: []\n")
            .expect_err("empty status list must fail");
    assert!(error.to_string().contains("nonempty list"));
}

/// Alerts retain their historical after-response/any selector when no `when`
/// block is configured.
#[test]
fn context_size_alert_default_selector_preserves_existing_behavior() {
    let alert: ContextSizeAlert =
        serde_yaml_ng::from_str("threshold: 100\nmessage: compact soon\n").expect("valid alert");
    assert_eq!(alert.when, context_size_alert_when_default());
}

/// Ensures the semantic effective threshold retains the established numeric
/// scalar encoding instead of exposing its refinement in config output.
#[test]
fn context_size_alert_threshold_serializes_as_numeric_scalar() {
    let alert: ContextSizeAlert =
        serde_yaml_ng::from_str("threshold: 100\nmessage: compact soon\n").expect("valid alert");
    assert_eq!(
        serde_json::to_value(&alert).expect("serialize effective alert")["threshold"],
        serde_json::json!(100)
    );
}
/// Ensures a role can inherit one named rule's timing and reset only its status
/// matcher without accidentally resetting the threshold or lifecycle point.
#[test]
fn named_compaction_layers_when_fields_and_null_resets() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path();
    std::fs::write(
        dir.join("harness.yaml"),
        r#"
agents:
  compactions:
    eager:
      threshold: 160000
      when:
        at: outer_turn_finished
        statuses: [done]
  role_groups:
    custom:
      roles:
        reviewer:
          compactions:
            eager:
              threshold: 180000
              when:
                statuses: null
"#,
    )
    .expect("write config");
    let settings = load_harness_settings_in(&dirs_with_config(dir)).expect("load settings");
    assert_eq!(
        settings.roles["reviewer"].compactions["eager"],
        CompactionPolicy {
            threshold: CompactionPolicyThreshold::Tokens(180_000),
            enable: true,
            when: ContextPolicyWhen {
                at: ContextPolicyPoint::OuterTurnFinished,
                statuses: None,
            },
        }
    );
}
/// Successor fields layer independently across sources: null clears the lower
/// legacy inference setting, while named fields merge and nested selector
/// fields reset or replace independently.
#[test]
fn successor_compaction_fields_merge_across_profile_sources() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        r#"
agents:
  inference_compaction: disabled
  compactions:
    eager:
      threshold: 40000
      enable: false
      when:
        at: outer_turn_finished
        statuses: [done, blocked]
    reset:
      threshold: 50000
      when:
        at: outer_turn_finished
        statuses: [done]
    any:
      threshold: 60000
      when:
        at: outer_turn_finished
        statuses: [done]
    dormant:
      threshold: 65000
profiles:
  selected:
    agents:
      inference_compaction: null
      compactions:
        eager:
          enable: true
          when:
            at: null
            statuses: [working]
        dormant:
          enable: false
        reset:
          when: null
        any:
          when:
            statuses: null
        camel:
          threshold: 70000
          when:
            at: outer_turn_finished
"#,
    )
    .expect("write layered config");
    let selected = profile_selection("selected");
    let settings = load_harness_settings_with_profile_and_cli_overrides_in(
        &dirs_with_config(td.path()),
        Some(&selected),
        &[],
        &[],
    )
    .expect("load selected profile");

    for role in settings.roles.values() {
        assert_eq!(
            role.inference_compaction,
            Some(RoleCompaction::ProviderDefault)
        );
        assert_eq!(
            role.compactions["eager"],
            CompactionPolicy {
                threshold: CompactionPolicyThreshold::Tokens(40_000),
                enable: true,
                when: ContextPolicyWhen {
                    at: ContextPolicyPoint::BeforeInference,
                    statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Working]),
                },
            }
        );
        assert!(!role.compactions["dormant"].enable);
        assert_eq!(role.compactions["reset"].when, ContextPolicyWhen::default());
        assert_eq!(
            role.compactions["any"].when,
            ContextPolicyWhen {
                at: ContextPolicyPoint::OuterTurnFinished,
                statuses: None,
            }
        );
        assert_eq!(
            role.compactions["camel"].when.at,
            ContextPolicyPoint::OuterTurnFinished
        );
    }
}
/// Disabling a rule does not make an incomplete, source-introduced named entry
/// valid because a later layer may re-enable it.
#[test]
fn disabled_compaction_without_inherited_threshold_fails_closed() {
    let td = TempDir::new().expect("tempdir");
    std::fs::write(
        td.path().join("harness.yaml"),
        "agents:\n  compactions:\n    dormant:\n      enable: false\n",
    )
    .expect("write incomplete config");
    let error = load_harness_settings_in(&dirs_with_config(td.path()))
        .expect_err("disabled incomplete rule must fail");
    assert!(
        error.to_string().contains("requires a threshold"),
        "unexpected error: {error}"
    );
}
