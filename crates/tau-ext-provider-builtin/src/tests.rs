use std::cell::Cell;
use std::collections::VecDeque;
use std::fs::File;
use std::sync::{Condvar, atomic as path_std_sync_atomic};
use std::{io as path_std_io, time as path_std_time};

use tau_proto::ProviderBackendKind;
use tau_provider_codex::oauth as path_tau_provider_codex_oauth;

mod compatibility;

use super::*;

/// Returns a parsed OAuth credential reference whose storage identity differs
/// from the provider names used by refresh-routing tests.
fn oauth_test_credential_reference() -> ProviderCredentialReference {
    ProviderCredentialReference::new(
        ProviderCredentialIdentity::parse("0123456789abcdef0123456789abcdef")
            .expect("valid opaque credential identity"),
        ProviderCredentialSlot::OAuth,
        None,
    )
    .expect("valid OAuth credential reference")
}

/// A route downgrade must stop automatic compaction while preserving explicit
/// identity-refresh admission only for the affected provider generation.
#[test]
fn compact_route_downgrade_republishes_honest_capability() {
    let unavailable = ProviderName::new("chatgpt");
    let available = ProviderName::new("other-chatgpt");
    let mut models = tau_provider_codex::models_for_provider(&unavailable);
    models.extend(tau_provider_codex::models_for_provider(&available));
    let identities = HashMap::from([(
        unavailable.clone(),
        InferenceProfileIdentity::from_test_value(1),
    )]);
    let unavailable_set = HashSet::from([InferenceProfileIdentity::from_test_value(1)]);

    apply_compact_route_downgrades(&mut models, &identities, &unavailable_set);

    assert!(
        models
            .iter()
            .filter(|model| {
                model.id.provider == unavailable && model.id.model.as_str().starts_with("gpt-5.6-")
            })
            .all(|model| !model.supports_standalone_compaction
                && model.standalone_compaction_generation_negative
                && model.supports_explicit_standalone_compaction()
                && model.standalone_compaction_threshold.is_none())
    );
    assert!(
        models
            .iter()
            .filter(|model| {
                model.id.provider == unavailable && !model.id.model.as_str().starts_with("gpt-5.6-")
            })
            .all(|model| !model.supports_standalone_compaction
                && !model.standalone_compaction_generation_negative
                && !model.supports_explicit_standalone_compaction())
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.provider == available)
            .any(|model| model.supports_standalone_compaction
                && !model.standalone_compaction_generation_negative
                && model.standalone_compaction_threshold.is_some())
    );
}

/// Pins the production prompt-worker default so ordinary provider instances
/// admit eight prompt jobs without an environment override.
#[test]
fn provider_prompt_concurrency_defaults_to_eight() {
    assert_eq!(DEFAULT_PROMPT_CONCURRENCY, 8);
}

/// Proves conflicting add source flags fail before any interactive or
/// credential-producing work.
#[test]
fn provider_add_rejects_conflicting_source_flags() {
    let network = tau_provider::OutboundNetworkPolicy::from_env();
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    let error = cmd_add(
        &["--config".to_owned(), "--state".to_owned()],
        &network,
        &extension,
    )
    .expect_err("conflicting flags");
    assert!(error.to_string().contains("exactly one source flag"));
}

/// Proves conflicting remove source flags cannot select a destructive target by
/// argv order.
#[test]
fn provider_remove_rejects_conflicting_source_flags() {
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    let error = cmd_remove(
        &[
            "--state".to_owned(),
            "--config".to_owned(),
            "profile".to_owned(),
        ],
        &extension,
    )
    .expect_err("conflicting flags");
    assert!(error.to_string().contains("exactly one source flag"));
}

/// Ensures rename exposes its required two-name CLI shape before touching local
/// storage.
#[test]
fn provider_rename_requires_old_and_new_names() {
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    let error = cmd_rename(&["old".to_owned()], &extension).expect_err("missing new name");
    assert!(
        error
            .to_string()
            .contains("tau provider rename requires OLD and NEW")
    );
    assert!(PROVIDER_CLI_HELP.contains("rename <old> <new>"));
}

fn cli_setup_plan(name: &str) -> setup_store::ProviderSetupPlan {
    let provider = ProviderName::try_new(name.to_owned()).expect("provider");
    let identity =
        ProviderCredentialIdentity::parse("0123456789abcdef0123456789abcdef").expect("identity");
    setup_store::ProviderSetupPlan {
        extension_instance: tau_proto::ExtensionName::parse("provider-builtin").expect("extension"),
        provider: provider.clone(),
        settings: serde_json::to_vec_pretty(&serde_json::json!({
            "kind": "responses",
            "base_url": "https://example.invalid/v1",
            "models": [{"id": "model"}],
            "max_output_tokens": 1024,
            "transport": "sse",
            "tags": [],
            "compat": {},
            "credential": {
                "kind": "api_key",
                "identity": identity.as_str()
            }
        }))
        .expect("settings"),
        credential: setup_store::CredentialSetup::Stored {
            secret: setup_store::SecretWrite {
                path: ProviderCredentialSlot::ApiKey.path(&identity),
                contents: setup_store::SecretBytes::new(
                    serde_json::to_vec(&credential_record::ApiKeyCredential::new("key".to_owned()))
                        .expect("credential"),
                ),
            },
            named_source: None,
        },
    }
}

/// Proves command-level list filtering keeps each parsed profile attached to
/// its source identity.
#[test]
fn provider_list_filters_and_displays_profile_sources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    store
        .apply_to(
            &cli_setup_plan("portable"),
            setup_store::ProfileTarget::Config,
        )
        .expect("config profile");
    store
        .apply_to(&cli_setup_plan("local"), setup_store::ProfileTarget::State)
        .expect("state profile");
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    let mut output = Vec::new();

    cmd_list_from_store(&["--config".to_owned()], &extension, &store, &mut output).expect("list");

    let output = String::from_utf8(output).expect("UTF-8 output");
    assert!(output.contains("\tportable\tresponses\t"));
    assert!(output.ends_with("\tconfig\n"));
    assert!(!output.contains("\tlocal\t"));
}

/// Proves absent and expired ChatGPT credentials carry the exact login command,
/// while a current credential omits remediation.
#[test]
fn provider_list_shows_actionable_chatgpt_login_remediation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    let provider = ProviderName::new("portable-chatgpt");
    let payload = provider_setup_payload(
        &provider,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth: OpenAiAuth {
                access_token: "fixture-access".to_owned(),
                refresh_token: "fixture-refresh".to_owned(),
                expires_at_ms: now_ms().saturating_add(86_400_000),
                account_id: None,
            },
            responses_lite_compatibility: false,
            cache_diagnostics: Default::default(),
        }),
        ProviderSetupInput::ProfileOAuth,
    )
    .expect("setup payload");
    let extension = tau_proto::ExtensionName::parse("provider-work").expect("extension");
    store
        .apply_to(
            &setup_store::ProviderSetupPlan {
                extension_instance: extension.clone(),
                provider: provider.clone(),
                settings: payload.settings,
                credential: setup_store::CredentialSetup::Keyless,
            },
            setup_store::ProfileTarget::Config,
        )
        .expect("credential-free config profile");
    let settings = store
        .snapshot(&extension)
        .expect("snapshot")
        .profiles
        .pop()
        .expect("profile")
        .contents;
    let expected_remediation =
        "login: tau provider --extension provider-work login portable-chatgpt";
    let (_, credential) =
        parse_settings_profile(&provider, &settings).expect("valid profile credential");
    let ProviderCredential::Stored(reference) = credential else {
        panic!("ChatGPT profile requires stored credentials");
    };
    for (expires_at_ms, expected_status, expects_remediation) in [
        (None, "not-configured", true),
        (Some(1), "expired", true),
        (Some(u64::MAX), "logged-in", false),
    ] {
        if let Some(expires_at_ms) = expires_at_ms {
            let record = credential_record::ChatGptOAuthCredential::from(OpenAiAuth {
                access_token: "fixture-access".to_owned(),
                refresh_token: "fixture-refresh".to_owned(),
                expires_at_ms,
                account_id: None,
            });
            store
                .publish_credential(
                    &extension,
                    &provider,
                    setup_store::ProfileSource::Config,
                    &settings,
                    &setup_store::SecretWrite {
                        path: ProviderCredentialSlot::OAuth.path(reference.identity()),
                        contents: setup_store::SecretBytes::new(
                            serde_json::to_vec(&record).expect("credential"),
                        ),
                    },
                    None,
                )
                .expect("credential publication");
        }
        let mut output = Vec::new();
        cmd_list_from_store(&[], &extension, &store, &mut output).expect("list");
        let output = String::from_utf8(output).expect("UTF-8 output");
        let row_prefix = format!(
            "provider-work\tportable-chatgpt\tchatgpt\t{expected_status}\tresponses-standard\tconfig"
        );
        assert!(output.starts_with(&row_prefix), "{output}");
        assert_eq!(
            output.contains(expected_remediation),
            expects_remediation,
            "{output}"
        );
    }
}

/// Proves bare add preflights outside a terminal: a sole config profile yields
/// exact login remediation, while a cross-source duplicate remains a collision,
/// all before OAuth or a state-profile write.
#[test]
fn provider_add_chatgpt_noninteractive_preflight_preserves_collision_safety() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    let provider = ProviderName::new("chatgpt");
    let payload = provider_setup_payload(
        &provider,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth: OpenAiAuth {
                access_token: "fixture-access".to_owned(),
                refresh_token: "fixture-refresh".to_owned(),
                expires_at_ms: u64::MAX,
                account_id: None,
            },
            responses_lite_compatibility: false,
            cache_diagnostics: Default::default(),
        }),
        ProviderSetupInput::ProfileOAuth,
    )
    .expect("setup payload");
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    store
        .apply_to(
            &setup_store::ProviderSetupPlan {
                extension_instance: extension.clone(),
                provider: provider.clone(),
                settings: payload.settings,
                credential: setup_store::CredentialSetup::Keyless,
            },
            setup_store::ProfileTarget::Config,
        )
        .expect("credential-free config profile");
    let network = tau_provider::OutboundNetworkPolicy::from_env();

    let error = cmd_add_chatgpt_in(
        &network,
        &extension,
        setup_store::ProfileTarget::State,
        true,
        &store,
        false,
    )
    .expect_err("noninteractive remediation");

    assert!(error.to_string().contains("tau provider login chatgpt"));
    assert!(
        !temp
            .path()
            .join("providers/provider-builtin/chatgpt.json")
            .exists()
    );

    let config = temp
        .path()
        .join("config/providers/provider-builtin/chatgpt.json");
    let state = temp.path().join("providers/provider-builtin/chatgpt.json");
    std::fs::write(&state, std::fs::read(config).expect("config settings"))
        .expect("duplicate state profile");
    let collision = cmd_add_chatgpt_in(
        &network,
        &extension,
        setup_store::ProfileTarget::State,
        true,
        &store,
        false,
    )
    .expect_err("cross-source collision");
    assert!(
        collision
            .to_string()
            .contains("duplicated across config and state")
    );
    assert!(!collision.to_string().contains("tau provider login"));
}

/// Proves remediation for a renamed provider instance keeps the exact
/// `--extension` selection instead of accidentally targeting the default.
#[test]
fn provider_login_remediation_preserves_extension_instance() {
    let extension = tau_proto::ExtensionName::parse("provider-work").expect("extension");
    let provider = ProviderName::new("chatgpt");

    assert_eq!(
        provider_login_command(&extension, &provider),
        "tau provider --extension provider-work login chatgpt"
    );
}

/// Proves show reports the owning source and host path without resolving any
/// credential value.
#[test]
fn provider_show_displays_source_path_and_credential_free_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    store
        .apply_to(
            &cli_setup_plan("portable"),
            setup_store::ProfileTarget::Config,
        )
        .expect("config profile");
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");
    let mut output = Vec::new();

    cmd_show_from_store(&["portable".to_owned()], &extension, &store, &mut output).expect("show");

    let output = String::from_utf8(output).expect("UTF-8 output");
    assert!(output.contains("source: config\npath: "));
    assert!(output.contains("config/providers/provider-builtin/portable.json"));
    assert!(output.contains("\"credential\""));
    assert!(!output.contains("\"value\""));
}

/// Proves malformed list entries fail with safe source and path identity rather
/// than disappearing from output.
#[test]
fn provider_list_reports_malformed_source_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    let path = temp
        .path()
        .join("config/providers/provider-builtin/bad.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("config root");
    std::fs::write(&path, b"not json").expect("malformed profile");
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");

    let error = cmd_list_from_store(&[], &extension, &store, &mut Vec::new())
        .expect_err("malformed profile");

    let error = error.to_string();
    assert!(error.contains("source=config"));
    assert!(error.contains(path.to_str().expect("UTF-8 path")));
    assert!(!error.contains("not json"));
}

/// Proves command-level discovery rejects an oversized external config profile
/// before allocating or parsing its contents.
#[cfg(unix)]
#[test]
fn provider_list_rejects_oversized_external_config_profile() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    let deployment = temp.path().join("oversized.json");
    let file = File::create(&deployment).expect("deployment");
    file.set_len(tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILE_BYTES + 1)
        .expect("oversized deployment");
    let profile = temp
        .path()
        .join("config/providers/provider-builtin/oversized.json");
    std::fs::create_dir_all(profile.parent().expect("profile parent")).expect("config root");
    symlink(deployment, profile).expect("profile symlink");
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");

    let error = cmd_list_from_store(&[], &extension, &store, &mut Vec::new())
        .expect_err("oversized profile");

    let error = error.to_string();
    assert!(error.contains("config profile"));
    assert!(error.contains("exceeds"));
}

/// Proves command-level discovery bounds the merged config/state entry count.
#[test]
fn provider_list_rejects_too_many_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path()).with_max_profile_files(4);
    let config_profiles = temp.path().join("config/providers/provider-builtin");
    let state_profiles = temp.path().join("providers/provider-builtin");
    std::fs::create_dir_all(&config_profiles).expect("config root");
    std::fs::create_dir_all(&state_profiles).expect("state root");
    for index in 0..2 {
        std::fs::write(config_profiles.join(format!("c-{index}.json")), b"{}").expect("profile");
    }
    for index in 0..3 {
        std::fs::write(state_profiles.join(format!("s-{index}.json")), b"{}").expect("profile");
    }
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");

    let error = cmd_list_from_store(&[], &extension, &store, &mut Vec::new())
        .expect_err("too many profiles")
        .downcast::<path_std_io::Error>()
        .expect("I/O error");

    assert_eq!(error.kind(), path_std_io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "state profile discovery exceeds 4 files");
}

/// Proves command-level discovery applies one aggregate byte budget across
/// config and state profiles.
#[test]
fn provider_list_rejects_merged_aggregate_profile_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = setup_store::SetupStore::open_in(temp.path());
    for (prefix, root) in [
        ("c", temp.path().join("config/providers/provider-builtin")),
        ("s", temp.path().join("providers/provider-builtin")),
    ] {
        std::fs::create_dir_all(&root).expect("profile root");
        for index in 0..8 {
            let file = File::create(root.join(format!("{prefix}-{index}.json"))).expect("profile");
            file.set_len(tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILE_BYTES)
                .expect("bounded profile");
        }
    }
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");

    let error =
        cmd_list_from_store(&[], &extension, &store, &mut Vec::new()).expect_err("aggregate bound");

    assert!(
        error
            .to_string()
            .contains("merged provider profile discovery exceeds")
    );
}

/// Proves dotfiles mode writes only canonical JSON to stdout and routes status
/// text exclusively to stderr.
#[test]
fn provider_dotfiles_output_separates_json_and_status() {
    let settings = br#"{
  "kind": "responses"
}"#;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    write_dotfiles_profile(
        settings,
        CredentialPublication::Published,
        &mut stdout,
        &mut stderr,
    )
    .expect("output");

    assert_eq!(stdout, [settings.as_slice(), b"\n"].concat());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stdout).expect("canonical JSON"),
        serde_json::json!({"kind": "responses"})
    );
    let stderr = String::from_utf8(stderr).expect("UTF-8 status");
    assert!(stderr.contains("deploy this config profile"));
    assert!(!stderr.contains("\"kind\""));
}
use crate::chat_completions::OpenRouterProfile;

/// Proves setup recognizes only canonical component suffix identity, never an
/// unrelated command or arbitrary argv token containing the component name.
#[test]
fn provider_setup_component_identity_is_exact() {
    let canonical = tau_config::settings::ExtensionEntry {
        role: Some("provider".to_owned()),
        suffix: Some(vec![
            "component".to_owned(),
            "ext-provider-builtin".to_owned(),
        ]),
        ..Default::default()
    };
    assert!(provider_cli_entry_is_builtin("provider-work", &canonical));

    for entry in [
        tau_config::settings::ExtensionEntry {
            role: Some("provider".to_owned()),
            command: Some(vec!["ext-provider-builtin".to_owned()]),
            suffix: Some(vec![
                "component".to_owned(),
                "ext-provider-builtin".to_owned(),
            ]),
            ..Default::default()
        },
        tau_config::settings::ExtensionEntry {
            role: Some("provider".to_owned()),
            suffix: Some(vec![
                "wrapper".to_owned(),
                "ext-provider-builtin".to_owned(),
            ]),
            ..Default::default()
        },
    ] {
        assert!(!provider_cli_entry_is_builtin("provider-work", &entry));
    }
}

/// Proves explicit keyless Chat Completions settings load without a Secret
/// path, while omission and unsupported keyless provider kinds fail closed.
#[test]
fn provider_settings_accept_only_explicit_supported_keyless_profiles() {
    let provider = ProviderName::new("local");
    let settings = serde_json::to_vec(&serde_json::json!({
        "kind": "chat_completions",
        "models": [{"id": "local-model"}],
        "credential": {"kind": "none"}
    }))
    .expect("keyless settings");
    let (profile, credential) =
        parse_settings_profile(&provider, &settings).expect("explicit keyless profile");
    assert!(matches!(
        profile,
        BuiltinProviderProfile::ChatCompletions(_)
    ));
    assert_eq!(credential, ProviderCredential::Keyless);
    let responses = serde_json::to_vec(&serde_json::json!({
        "kind": "responses",
        "base_url": "http://localhost:8080/v1",
        "models": [{"id": "local-model"}],
        "credential": {"kind": "none"}
    }))
    .expect("keyless Responses settings");
    assert!(matches!(
        parse_settings_profile(&provider, &responses),
        Ok((
            BuiltinProviderProfile::Responses(_),
            ProviderCredential::Keyless
        ))
    ));

    for invalid in [
        serde_json::json!({
            "kind": "chat_completions",
            "models": [{"id": "local-model"}]
        }),
        serde_json::json!({
            "kind": "openrouter",
            "models": [{"id": "remote-model"}],
            "credential": {"kind": "none"}
        }),
        serde_json::json!({
            "kind": "chatgpt",
            "credential": {"kind": "none"}
        }),
    ] {
        assert!(
            parse_settings_profile(
                &provider,
                &serde_json::to_vec(&invalid).expect("invalid settings")
            )
            .is_err()
        );
    }
}

/// Proves ChatGPT's explicit OAuth setup input publishes acquired credentials
/// and can never serialize the API-profile keyless marker.
#[test]
fn chatgpt_setup_keeps_oauth_credential_publication() {
    let provider = ProviderName::new("chatgpt");
    let payload = provider_setup_payload(
        &provider,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth: OpenAiAuth {
                access_token: "fixture-access".to_owned(),
                refresh_token: "fixture-refresh".to_owned(),
                expires_at_ms: 1,
                account_id: None,
            },
            responses_lite_compatibility: false,
            cache_diagnostics: Default::default(),
        }),
        ProviderSetupInput::ProfileOAuth,
    )
    .expect("ChatGPT setup payload");
    assert!(matches!(
        payload.credential,
        setup_store::CredentialSetup::Stored { .. }
    ));
    let settings: serde_json::Value =
        serde_json::from_slice(&payload.settings).expect("ChatGPT settings");
    assert_eq!(settings["credential"]["kind"], "oauth");
}

/// Proves keyless setup emits the explicit portable marker and never plans a
/// dummy Secret record.
#[test]
fn keyless_setup_requires_no_secret_publication() {
    let provider = ProviderName::new("local");
    let profile = BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
        cache_diagnostics: Default::default(),
        base_url: "http://localhost:8080/v1".to_owned(),
        api_key: String::new(),
        models: vec![test_chat_model("local-model")],
        max_output_tokens: tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        tags: Vec::new(),
        compat: ChatCompletionsCompat::default(),
    });
    let payload = provider_setup_payload(
        &provider,
        &profile,
        ProviderSetupInput::ApiKey(ApiKeySource::Keyless),
    )
    .expect("keyless payload");
    assert!(matches!(
        payload.credential,
        setup_store::CredentialSetup::Keyless
    ));
    let settings: serde_json::Value =
        serde_json::from_slice(&payload.settings).expect("keyless settings");
    assert_eq!(settings["credential"], serde_json::json!({"kind": "none"}));
}

/// Proves an explicit deferred source emits a closed typed reference but never
/// constructs a placeholder credential publication plan.
#[test]
fn deferred_named_setup_writes_only_credential_free_binding() {
    let provider = ProviderName::new("future");
    let profile = BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
        cache_diagnostics: Default::default(),
        base_url: "https://example.invalid/v1".to_owned(),
        api_key: String::new(),
        models: vec![test_chat_model("model")],
        max_output_tokens: tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        tags: Vec::new(),
        compat: ChatCompletionsCompat::default(),
    });

    let payload = provider_setup_payload(
        &provider,
        &profile,
        ProviderSetupInput::ApiKey(ApiKeySource::DeferredNamed {
            name: "Future_Key".to_owned(),
        }),
    )
    .expect("deferred setup payload");

    let setup_store::CredentialSetup::DeferredNamed { path } = payload.credential else {
        panic!("deferred source must not publish a credential");
    };
    assert!(path.as_str().ends_with("/api-key.json"));
    let settings: serde_json::Value = serde_json::from_slice(&payload.settings).expect("settings");
    assert_eq!(
        settings["credential"]["source"],
        serde_json::json!({"kind": "named_secret", "name": "Future_Key"})
    );
}

/// Proves an explicit deferred binding accepts an exact configured name while
/// retaining the shared source-name grammar.
#[test]
fn deferred_named_setup_accepts_configured_name_but_rejects_invalid_name() {
    assert!(validate_deferred_secret_name("Future.Key-1").is_ok());
    assert!(validate_deferred_secret_name("configured_key").is_ok());
    assert!(validate_deferred_secret_name("CONFIGURED_KEY").is_ok());
    assert!(validate_deferred_secret_name("../bad").is_err());
}

/// Proves stdout status distinguishes a portable deferred binding from both a
/// host-local credential publication and explicit keyless operation.
#[test]
fn deferred_dotfiles_output_reports_no_host_secret_write() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    write_dotfiles_profile(
        br#"{"kind":"responses"}"#,
        CredentialPublication::Deferred,
        &mut stdout,
        &mut stderr,
    )
    .expect("output");

    assert!(
        String::from_utf8(stderr)
            .expect("status")
            .contains("declare and provide")
    );
}

/// Proves the canonical built-in name inherits identity only while its command
/// remains inherited; an explicit replacement cannot claim built-in authority.
#[test]
fn default_provider_setup_identity_rejects_command_replacement_without_suffix() {
    assert!(provider_cli_entry_is_builtin(
        "provider-builtin",
        &tau_config::settings::ExtensionEntry::default(),
    ));
    assert!(!provider_cli_entry_is_builtin(
        "provider-builtin",
        &tau_config::settings::ExtensionEntry {
            command: Some(vec!["provider-builtin".to_owned()]),
            ..Default::default()
        },
    ));
}

/// Proves list/status treats direct values as active and every empty,
/// malformed, missing, or orphan-like API-key record as inactive.
#[test]
fn setup_api_key_status_is_closed_and_value_aware() {
    let identity =
        ProviderCredentialIdentity::parse("0123456789abcdef0123456789abcdef").expect("identity");
    let credential = ProviderCredential::Stored(
        ProviderCredentialReference::new(identity.clone(), ProviderCredentialSlot::ApiKey, None)
            .expect("credential reference"),
    );
    let key = (identity, ProviderCredentialSlot::ApiKey);
    for (record, expected) in [
        (
            Some(br#"{"version":0,"kind":"api_key","value":"active"}"#.to_vec()),
            "api-key",
        ),
        (
            Some(br#"{"version":0,"kind":"api_key","value":""}"#.to_vec()),
            "no-api-key",
        ),
        (Some(b"malformed".to_vec()), "no-api-key"),
        (None, "no-api-key"),
    ] {
        let credentials = record
            .map(|record| BTreeMap::from([(key.clone(), record)]))
            .unwrap_or_default();
        assert_eq!(setup_api_key_status(&credentials, &credential), expected);
    }
}

/// Proves unsupported credential versions fail during deserialization before
/// they can enter provider runtime state.
#[test]
fn credential_records_validate_versions_while_decoding() {
    let oauth = br#"{"version":1,"kind":"chatgpt_oauth","access_token":"a","refresh_token":"r","expires_at_ms":1,"account_id":null}"#;
    let api_key = br#"{"version":1,"kind":"api_key","value":"k"}"#;

    let oauth_error =
        match serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(oauth) {
            Err(error) => error,
            Ok(_) => panic!("unsupported OAuth version decoded"),
        };
    let api_key_error = match serde_json::from_slice::<credential_record::ApiKeyCredential>(api_key)
    {
        Err(error) => error,
        Ok(_) => panic!("unsupported API-key version decoded"),
    };
    assert!(
        oauth_error
            .to_string()
            .contains("unsupported ChatGPT OAuth")
    );
    assert!(api_key_error.to_string().contains("unsupported API-key"));
}

/// Proves settings cannot redirect credential hydration to another provider or
/// select a slot inconsistent with the provider kind.
#[test]
fn provider_settings_credential_reference_is_authoritative_and_exact() {
    let provider = ProviderName::new("chatgpt");
    for settings in [
        br#"{"kind":"chatgpt","credential":{"kind":"oauth","secret_path":"providers/other/oauth.json"}}"#
            .as_slice(),
        br#"{"kind":"chatgpt","credential":{"kind":"api_key","secret_path":"providers/chatgpt/api-key.json"}}"#
            .as_slice(),
    ] {
        assert!(parse_settings_profile(&provider, settings).is_err());
    }
}

/// Ensures initial profile hydration retains an opaque credential path instead
/// of reconstructing it from the user-facing provider namespace.
#[test]
fn chatgpt_initial_hydration_retains_credential_identity() {
    let provider = ProviderName::new("chatgpt-fedi");
    let profiles = try_load_settings_profiles(vec![(
        provider.clone(),
        br#"{
            "kind": "chatgpt",
            "credential": {
                "kind": "oauth",
                "identity": "0123456789abcdef0123456789abcdef"
            }
        }"#
        .to_vec(),
    )])
    .expect("credential-free ChatGPT settings");

    assert_eq!(
        profiles
            .chatgpt_credential_reference(&provider)
            .expect("parsed OAuth credential reference")
            .path()
            .as_str(),
        "providers/0123456789abcdef0123456789abcdef/oauth.json"
    );
}

fn configured_chat_completions_settings(_provider: &str, extra: serde_json::Value) -> Vec<u8> {
    let mut settings = serde_json::json!({
        "kind": "chat_completions",
        "models": [{"id": "deepseek-chat"}],
        "credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef"
        }
    });
    settings
        .as_object_mut()
        .expect("test settings object")
        .extend(extra.as_object().expect("test extra object").clone());
    serde_json::to_vec(&settings).expect("serialize test settings")
}

fn run_provider_configure(
    settings_files: BTreeMap<String, Vec<u8>>,
) -> (
    Result<(), Box<dyn Error>>,
    Vec<tau_proto::HarnessInputMessage>,
) {
    let mut input = Vec::new();
    {
        let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
        writer
            .write_message(&tau_proto::HarnessOutputMessage::Configure(
                tau_proto::Configure {
                    tool_prefix: None,
                    config: tau_proto::CborValue::Map(Vec::new()),
                    instance_name: tau_proto::ExtensionName::parse("provider-builtin")
                        .expect("extension name"),
                    state_dir: None,
                    secrets: BTreeMap::new(),
                    settings_files,
                },
            ))
            .expect("encode Configure");
        writer.flush().expect("flush Configure");
    }
    let output = SharedTraceWriter::default();
    let result = run(Cursor::new(input), output.clone());
    (result, decode_frames(&output.bytes()))
}

/// Drives the production Configure and Secret-RPC transport, returning every
/// credential request, declaration, and Ready marker in wire order.
fn run_production_credential_scenario(
    credential_rounds: Vec<(Vec<ProductionCredentialReply>, bool)>,
) -> Vec<ProductionCredentialTrace> {
    run_production_credential_scenario_with(
        configured_chat_completions_settings("deepseek", serde_json::json!({})),
        tau_proto::ModelId::new(
            ProviderName::new("deepseek"),
            tau_proto::ModelName::new("deepseek-chat"),
        ),
        "providers/0123456789abcdef0123456789abcdef/api-key.json",
        credential_rounds,
    )
}

/// Runs the production credential oracle with one configured profile and model.
fn run_production_credential_scenario_with(
    settings: Vec<u8>,
    model: tau_proto::ModelId,
    credential_path: &str,
    credential_rounds: Vec<(Vec<ProductionCredentialReply>, bool)>,
) -> Vec<ProductionCredentialTrace> {
    use std::os::unix::net::UnixStream;

    let (extension_socket, harness_socket) = UnixStream::pair().expect("provider socket pair");
    let extension_reader = extension_socket.try_clone().expect("clone provider reader");
    let provider = std::thread::spawn(move || {
        run(extension_reader, extension_socket).map_err(|error| error.to_string())
    });
    let harness_reader = harness_socket.try_clone().expect("clone harness reader");
    let harness_timeout = harness_reader
        .try_clone()
        .expect("clone harness timeout control");
    harness_reader
        .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
        .expect("set harness read timeout");
    let mut reader = tau_proto::HarnessInputReader::new(harness_reader);
    let mut writer = tau_proto::HarnessOutputWriter::new(harness_socket);

    assert!(matches!(
        reader.read_message().expect("provider Hello"),
        Some(HarnessInputMessage::Hello(_))
    ));
    let settings_file = format!("{}.json", model.provider);
    writer
        .write_message(&tau_proto::HarnessOutputMessage::Configure(
            tau_proto::Configure {
                tool_prefix: None,
                config: tau_proto::CborValue::Map(Vec::new()),
                instance_name: tau_proto::ExtensionName::parse("provider-builtin")
                    .expect("extension name"),
                state_dir: None,
                secrets: BTreeMap::new(),
                settings_files: BTreeMap::from([(settings_file, settings)]),
            },
        ))
        .expect("write Configure");
    writer.flush().expect("flush Configure");

    let mut rounds = credential_rounds.into_iter();
    let (startup_replies, _) = rounds.next().expect("startup credential round");
    let mut startup_replies = VecDeque::from(startup_replies);
    let mut trace = Vec::new();
    let mut ready = false;
    let mut requested = 0usize;
    while !ready {
        let message = reader
            .read_message()
            .expect("read provider startup output")
            .expect("provider startup remains connected");
        match message {
            HarnessInputMessage::ExtensionDataRequest(request) => {
                assert_production_credential_read(&request, credential_path);
                requested += 1;
                trace.push(ProductionCredentialTrace::SecretRequest);
                let reply = startup_replies
                    .pop_front()
                    .expect("startup credential reply for request");
                writer
                    .write_message(&tau_proto::HarnessOutputMessage::ExtensionDataResult(
                        Box::new(tau_proto::ExtensionDataResult {
                            request_id: request.request_id,
                            result: reply.into_payload(),
                        }),
                    ))
                    .expect("write Secret result");
                writer.flush().expect("flush Secret result");
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::ProviderModelsDeclared(declaration) = emit.event.as_ref() {
                    trace.push(ProductionCredentialTrace::Declaration(
                        declaration.models.clone(),
                    ));
                }
            }
            HarnessInputMessage::Ready(_) => {
                trace.push(ProductionCredentialTrace::Ready);
                ready = true;
            }
            _ => {}
        }
    }

    assert!(startup_replies.is_empty());
    for (replies, expect_declaration) in rounds {
        let mut replies = VecDeque::from(replies);
        writer
            .write_message(&tau_proto::HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(1),
                Event::AgentPromptPrewarmRequested(tau_proto::AgentPromptPrewarmRequested {
                    agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
                    session_id: tau_proto::SessionId::parse("session").expect("session id"),
                    system_prompt: String::new(),
                    context: tau_proto::PromptContext::default(),
                    tools: Vec::new(),
                    model: Some(model.clone()),
                    model_params: Default::default(),
                    tool_choice: Default::default(),
                    originator: tau_proto::PromptOriginator::User,
                    share_user_cache_key: false,
                }),
            ))
            .expect("write prewarm");
        writer.flush().expect("flush prewarm");
        let mut saw_request = false;
        loop {
            let message = reader.read_message().unwrap_or_else(|error| {
                panic!(
                    "read credential refresh output after {trace:?}, pending replies {}: {error}",
                    replies.len()
                )
            }).expect("provider refresh remains connected");
            match message {
                HarnessInputMessage::ExtensionDataRequest(request) => {
                    assert_production_credential_read(&request, credential_path);
                    saw_request = true;
                    requested += 1;
                    trace.push(ProductionCredentialTrace::SecretRequest);
                    writer
                        .write_message(&tau_proto::HarnessOutputMessage::ExtensionDataResult(
                            Box::new(tau_proto::ExtensionDataResult {
                                request_id: request.request_id,
                                result: replies
                                    .pop_front()
                                    .expect("credential reply for request")
                                    .into_payload(),
                            }),
                        ))
                        .expect("write replacement Secret result");
                    writer.flush().expect("flush replacement Secret result");
                    if !expect_declaration && replies.is_empty() {
                        break;
                    }
                }
                HarnessInputMessage::Emit(emit) => {
                    if let Event::ProviderModelsDeclared(declaration) = emit.event.as_ref() {
                        assert!(
                            saw_request,
                            "a declaration crossed the next Secret-request barrier"
                        );
                        trace.push(ProductionCredentialTrace::Declaration(
                            declaration.models.clone(),
                        ));
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(replies.is_empty());
    }
    assert_eq!(
        requested,
        trace
            .iter()
            .filter(|item| matches!(item, ProductionCredentialTrace::SecretRequest))
            .count()
    );
    drop(writer);
    drop(reader);
    drop(harness_timeout);
    provider
        .join()
        .expect("join provider")
        .expect("provider exits on harness EOF");
    trace
}

/// Verifies the production provider asks only for its configured Secret record.
fn assert_production_credential_read(
    request: &tau_proto::ExtensionDataRequest,
    credential_path: &str,
) {
    assert_eq!(request.scope, tau_proto::ExtensionDataScope::Secret);
    assert!(request.expected_session_id.is_none());
    assert!(matches!(
        &request.op,
        tau_proto::ExtensionDataRequestOp::ReadFile { path }
            if path.as_str() == credential_path
    ));
}

/// Proves the production prompt path accepts out-of-order Secret replies
/// asynchronously but emits submitted ownership in original prompt order.
#[test]
fn production_prompt_secret_replies_preserve_admission_fifo() {
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;

    let provider_name = ProviderName::new("deepseek");
    let model = tau_proto::ModelId::new(
        provider_name.clone(),
        tau_proto::ModelName::new("deepseek-chat"),
    );
    let credential_path = "providers/0123456789abcdef0123456789abcdef/api-key.json";
    let (extension_socket, harness_socket) = UnixStream::pair().expect("provider socket pair");
    let extension_reader = extension_socket.try_clone().expect("clone provider reader");
    let receipt_trace = SharedTraceWriter::default();
    let receipt_subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.receipt=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let receipt_trace = receipt_trace.clone();
            move || receipt_trace.clone()
        })
        .finish();
    let receipt_dispatch = tracing::Dispatch::new(receipt_subscriber);
    let provider = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&receipt_dispatch, || {
            run(extension_reader, extension_socket).map_err(|error| error.to_string())
        })
    });
    let harness_reader = harness_socket.try_clone().expect("clone harness reader");
    harness_reader
        .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
        .expect("set harness timeout");
    let mut reader = tau_proto::HarnessInputReader::new(harness_reader);
    let mut writer = tau_proto::HarnessOutputWriter::new(harness_socket);

    assert!(matches!(
        reader.read_message().expect("provider Hello"),
        Some(HarnessInputMessage::Hello(_))
    ));
    writer
        .write_message(&tau_proto::HarnessOutputMessage::Configure(
            tau_proto::Configure {
                tool_prefix: None,
                config: tau_proto::CborValue::Map(Vec::new()),
                instance_name: tau_proto::ExtensionName::parse("provider-builtin")
                    .expect("extension name"),
                state_dir: None,
                secrets: BTreeMap::new(),
                settings_files: BTreeMap::from([(
                    "deepseek.json".to_owned(),
                    configured_chat_completions_settings("deepseek", serde_json::json!({})),
                )]),
            },
        ))
        .expect("write Configure");
    writer.flush().expect("flush Configure");

    loop {
        match reader
            .read_message()
            .expect("read startup")
            .expect("provider connected")
        {
            HarnessInputMessage::ExtensionDataRequest(request) => {
                assert_production_credential_read(&request, credential_path);
                writer
                    .write_message(&tau_proto::HarnessOutputMessage::ExtensionDataResult(
                        Box::new(tau_proto::ExtensionDataResult {
                            request_id: request.request_id,
                            result: ProductionCredentialReply::Missing.into_payload(),
                        }),
                    ))
                    .expect("write missing startup credential");
                writer.flush().expect("flush startup credential");
            }
            HarnessInputMessage::Ready(_) => break,
            _ => {}
        }
    }

    let mut first = super::openai_tests::prompt();
    first.agent_prompt_id = "fifo-1".parse().expect("prompt id");
    first.model = model.clone();
    let mut second = first.clone();
    second.agent_prompt_id = "fifo-2".parse().expect("prompt id");
    for prompt in [first, second] {
        writer
            .write_message(&tau_proto::HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(1),
                Event::AgentPromptCreated(prompt),
            ))
            .expect("write prompt");
    }
    writer.flush().expect("flush prompts");

    let mut requests = Vec::new();
    while requests.len() < 2 {
        if let HarnessInputMessage::ExtensionDataRequest(request) = reader
            .read_message()
            .expect("read credential request")
            .expect("provider connected")
        {
            assert_production_credential_read(&request, credential_path);
            requests.push(request.request_id);
        }
    }
    for request_id in requests.into_iter().rev() {
        let malformed_secret = b"secret-value-canary".to_vec();
        writer
            .write_message(&tau_proto::HarnessOutputMessage::ExtensionDataResult(
                Box::new(tau_proto::ExtensionDataResult {
                    request_id,
                    result: ProductionCredentialReply::Contents(malformed_secret).into_payload(),
                }),
            ))
            .expect("write out-of-order credential result");
        writer.flush().expect("flush credential result");
    }

    let mut submitted = Vec::new();
    while submitted.len() < 2 {
        if let HarnessInputMessage::Emit(emit) = reader
            .read_message()
            .expect("read submitted report")
            .expect("provider connected")
            && let Event::ProviderPromptSubmittedReported(report) = emit.event.as_ref()
        {
            submitted.push(report.agent_prompt_id.to_string());
        }
    }
    assert_eq!(submitted, ["fifo-1", "fifo-2"]);

    // Submitted reports precede worker start. Wait for both independently
    // owned receipt observations before closing the harness; otherwise shutdown
    // can race the queued second worker and make this trace assertion flaky.
    assert!(
        receipt_trace.wait_for_occurrences(
            b"provider receipt observation",
            2,
            path_std_time::Duration::from_secs(2),
        ),
        "both receipt observations must publish before shutdown"
    );

    drop(writer);
    drop(reader);
    let result = provider.join().expect("join provider");
    assert!(
        result
            .as_ref()
            .map_or_else(|error| error.contains("Broken pipe"), |_| true),
        "provider exits after harness close: {result:?}"
    );
    let receipt_trace = String::from_utf8(receipt_trace.bytes()).expect("receipt trace UTF-8");
    let receipt_lines = receipt_trace
        .lines()
        .filter(|line| line.contains("provider receipt observation"))
        .collect::<Vec<_>>();
    assert_eq!(receipt_lines.len(), 2, "{receipt_trace}");
    for receipt in receipt_lines {
        assert!(receipt.contains("secret_rpc_count=1"), "{receipt}");
        assert!(receipt.contains("secret_bytes=19"), "{receipt}");
        assert!(!receipt.contains("secret-value-canary"), "{receipt}");
        assert!(!receipt.contains("fifo-"), "{receipt}");
    }
}

/// Ordered production transport observations relevant to credential
/// declarations.
#[derive(Debug)]
enum ProductionCredentialTrace {
    /// The provider requested its configured Secret record.
    SecretRequest,
    /// The provider emitted this complete replacement model declaration.
    Declaration(Vec<ProviderModelInfo>),
    /// The provider crossed its startup Ready boundary.
    Ready,
}

/// Secret result supplied by the production transport oracle.
#[derive(Clone, Debug)]
enum ProductionCredentialReply {
    /// Secret storage returned these exact bytes.
    Contents(Vec<u8>),
    /// Secret storage reported that the configured record does not exist.
    Missing,
}

impl ProductionCredentialReply {
    /// Converts the fixture reply into the real extension-data result payload.
    fn into_payload(self) -> tau_proto::ExtensionDataResultPayload {
        match self {
            Self::Contents(contents) => tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::ReadFile { contents },
            },
            Self::Missing => tau_proto::ExtensionDataResultPayload::Error {
                kind: tau_proto::ExtensionDataErrorKind::NotFound,
                message: "fixture credential is absent".to_owned(),
            },
        }
    }
}

/// Proves successful production Secret hydration precedes the initial
/// declaration and Ready, then a changed usable generation publishes a full
/// replacement with the same model list.
#[test]
fn production_credential_hydration_and_replacement_preserve_wire_order() {
    let credential_record = |value: &str| {
        serde_json::to_vec(&credential_record::ApiKeyCredential::new(value.to_owned()))
            .expect("credential record")
    };
    let trace = run_production_credential_scenario(vec![
        (
            vec![ProductionCredentialReply::Contents(credential_record(
                "locally-usable-not-remotely-verified-a",
            ))],
            true,
        ),
        (
            vec![ProductionCredentialReply::Contents(credential_record(
                "locally-usable-not-remotely-verified-b",
            ))],
            true,
        ),
    ]);

    assert!(matches!(
        trace.as_slice(),
        [
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(initial),
            ProductionCredentialTrace::Ready,
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(replacement),
        ] if !initial.is_empty() && initial == replacement
    ));
}

/// Proves a real ChatGPT resolver is followed by an authoritative Secret
/// reread whose changed OAuth generation publishes a replacement declaration.
#[test]
fn production_oauth_resolution_publishes_authoritative_generation() {
    let oauth_record = |account: &str| {
        serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
            OpenAiAuth {
                access_token: oauth_test_jwt(account),
                refresh_token: format!("{account}-refresh"),
                expires_at_ms: u64::MAX,
                account_id: Some(account.to_owned()),
            },
        ))
        .expect("OAuth credential record")
    };
    let initial = oauth_record("account-a");
    let authoritative = oauth_record("account-b");
    let settings = br#"{
        "kind": "chatgpt",
        "credential": {
            "kind": "oauth",
            "identity": "0123456789abcdef0123456789abcdef"
        }
    }"#
    .to_vec();
    let trace = run_production_credential_scenario_with(
        settings,
        tau_proto::ModelId::new(
            ProviderName::new("chatgpt"),
            tau_proto::ModelName::new("gpt-5.3-codex"),
        ),
        "providers/0123456789abcdef0123456789abcdef/oauth.json",
        vec![
            (
                vec![ProductionCredentialReply::Contents(initial.clone())],
                true,
            ),
            (
                vec![
                    ProductionCredentialReply::Contents(initial),
                    ProductionCredentialReply::Contents(authoritative),
                ],
                true,
            ),
        ],
    );

    assert!(matches!(
        trace.as_slice(),
        [
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(initial_models),
            ProductionCredentialTrace::Ready,
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(replacement_models),
        ] if !initial_models.is_empty() && initial_models == replacement_models
    ));
}

/// Proves a malformed production Secret reply yields an empty declaration
/// after the request and before Ready.
#[test]
fn production_malformed_credential_is_excluded_before_ready() {
    let trace = run_production_credential_scenario(vec![(
        vec![ProductionCredentialReply::Contents(b"not-json".to_vec())],
        true,
    )]);

    assert!(matches!(
        trace.as_slice(),
        [
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(models),
            ProductionCredentialTrace::Ready,
        ] if models.is_empty()
    ));
}

/// Proves an explicit production NotFound response yields an empty declaration
/// after the Secret request and before Ready.
#[test]
fn production_missing_credential_is_excluded_before_ready() {
    let trace =
        run_production_credential_scenario(vec![(vec![ProductionCredentialReply::Missing], true)]);

    assert!(matches!(
        trace.as_slice(),
        [
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(models),
            ProductionCredentialTrace::Ready,
        ] if models.is_empty()
    ));
}

/// Proves an unchanged production credential observation does not emit a
/// redundant replacement declaration.
#[test]
fn production_unchanged_credential_observation_deduplicates_declaration() {
    let record = serde_json::to_vec(&credential_record::ApiKeyCredential::new(
        "same-generation".to_owned(),
    ))
    .expect("credential record");
    let trace = run_production_credential_scenario(vec![
        (
            vec![ProductionCredentialReply::Contents(record.clone())],
            true,
        ),
        (vec![ProductionCredentialReply::Contents(record)], false),
        (
            vec![ProductionCredentialReply::Contents(
                serde_json::to_vec(&credential_record::ApiKeyCredential::new(
                    "barrier-generation".to_owned(),
                ))
                .expect("barrier credential record"),
            )],
            true,
        ),
    ]);

    assert!(matches!(
        trace.as_slice(),
        [
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(_),
            ProductionCredentialTrace::Ready,
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::SecretRequest,
            ProductionCredentialTrace::Declaration(_),
        ]
    ));
}

/// Proves one invalid profile rejects the complete initial settings generation
/// instead of retaining valid profiles parsed before the failure.
#[test]
fn provider_settings_snapshot_validation_is_atomic() {
    let valid = configured_chat_completions_settings("valid", serde_json::json!({}));
    let invalid = configured_chat_completions_settings(
        "deepseek",
        serde_json::json!({"api_key_secret": "legacy-secret-name"}),
    );
    let error = try_load_settings_profiles(vec![
        (ProviderName::new("valid"), valid),
        (ProviderName::new("deepseek"), invalid),
    ])
    .expect_err("legacy credential field must reject the complete snapshot");

    assert_eq!(error.provider, ProviderName::new("deepseek"));
    assert_eq!(
        error.reason,
        ProviderSettingsValidationReason::CredentialFieldsPresent
    );
}

/// Removed summary fields must produce exact migration guidance for both
/// compatible wire families instead of serde's generic unknown-field error.
#[test]
fn local_summary_compaction_obsolete_fields_have_actionable_migration_errors() {
    let cases = [
        (
            "chat",
            serde_json::json!({
                "kind": "chat_completions",
                "models": [{
                    "id": "local",
                    "local_summary_compaction": {
                        "serialization_profile": "local_transcript_v1"
                    }
                }],
                "credential": {"kind": "none"}
            }),
            ProviderSettingsValidationReason::ObsoleteLocalSummarySerializationProfile,
        ),
        (
            "responses",
            serde_json::json!({
                "kind": "responses",
                "models": [{
                    "id": "local",
                    "local_summary_compaction": {
                        "context_window_tokens": 128000
                    }
                }],
                "credential": {"kind": "none"}
            }),
            ProviderSettingsValidationReason::ObsoleteLocalSummaryContextWindow,
        ),
        (
            "openrouter",
            serde_json::json!({
                "kind": "openrouter",
                "models": [{
                    "id": "remote",
                    "local_summary_compaction": {
                        "context_window_tokens": 128000
                    }
                }],
                "credential": {
                    "kind": "api_key",
                    "identity": "0123456789abcdef0123456789abcdef"
                }
            }),
            ProviderSettingsValidationReason::ObsoleteLocalSummaryContextWindow,
        ),
    ];

    for (name, settings, expected) in cases {
        let error = try_load_settings_profiles(vec![(
            ProviderName::new(name),
            serde_json::to_vec(&settings).expect("settings JSON"),
        )])
        .expect_err("obsolete field must reject the profile");
        assert_eq!(error.reason, expected);
    }
}

/// Omission and an empty override object must publish the same generic fallback
/// for each compatible provider profile kind.
#[test]
fn local_summary_compaction_omission_and_empty_object_match_for_all_provider_kinds() {
    for kind in ["chat_completions", "openrouter", "responses"] {
        let credential = if kind == "openrouter" {
            serde_json::json!({
                "kind": "api_key",
                "identity": "0123456789abcdef0123456789abcdef"
            })
        } else {
            serde_json::json!({"kind": "none"})
        };
        let settings = serde_json::json!({
            "kind": kind,
            "models": [
                {"id": "omitted", "context_window": 8192},
                {
                    "id": "empty",
                    "context_window": 8192,
                    "local_summary_compaction": {}
                }
            ],
            "credential": credential
        });
        let provider_name = ProviderName::new(kind);
        let (profile, _) = parse_settings_profile(
            &provider_name,
            &serde_json::to_vec(&settings).expect("settings JSON"),
        )
        .expect("compatible profile");
        let models = match profile {
            BuiltinProviderProfile::ChatCompletions(provider) => {
                chat_models_for_provider(&provider_name, &provider)
            }
            BuiltinProviderProfile::OpenRouter(profile) => {
                chat_models_for_provider(&provider_name, &profile.to_chat_completions())
            }
            BuiltinProviderProfile::Responses(provider) => {
                responses::models_for_provider(&provider_name, &provider)
            }
            BuiltinProviderProfile::Chatgpt(_) => panic!("unexpected profile kind"),
        };

        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .all(|model| model.supports_standalone_compaction)
        );
        assert_eq!(
            models[0].standalone_compaction_threshold,
            models[1].standalone_compaction_threshold
        );
        assert_eq!(
            models[0].standalone_compaction_prefix_budget,
            models[1].standalone_compaction_prefix_budget
        );
    }
}

/// Cross-field summary mistakes must reject the complete startup snapshot with
/// one closed field-specific reason for both compatible wire families.
#[test]
fn local_summary_compaction_invalid_bounds_reject_profile_snapshot() {
    let cases = [
        (
            "chat",
            serde_json::json!({
                "kind": "chat_completions",
                "models": [{
                    "id": "local",
                    "context_window": 8,
                    "local_summary_compaction": {"max_output_tokens": 9}
                }],
                "credential": {"kind": "none"}
            }),
            ProviderSettingsValidationReason::LocalSummaryOutputTokensExceedContextWindow,
        ),
        (
            "responses",
            serde_json::json!({
                "kind": "responses",
                "models": [{
                    "id": "local",
                    "local_summary_compaction": {"max_output_bytes": 262145}
                }],
                "credential": {"kind": "none"}
            }),
            ProviderSettingsValidationReason::LocalSummaryOutputBytesExceedNarrativeLimit,
        ),
    ];

    for (name, settings, expected) in cases {
        let error = try_load_settings_profiles(vec![(
            ProviderName::new(name),
            serde_json::to_vec(&settings).expect("settings JSON"),
        )])
        .expect_err("invalid summary bound must reject the profile");
        assert_eq!(error.reason, expected);
    }
}

/// Proves a portable explicit keyless profile crosses Configure and publishes
/// its model without issuing a Secret request or requiring a dummy record.
#[test]
fn keyless_provider_configure_publishes_models_without_secret_state() {
    let settings = serde_json::to_vec(&serde_json::json!({
        "kind": "chat_completions",
        "models": [{"id": "local-model"}],
        "credential": {"kind": "none"}
    }))
    .expect("keyless settings");
    let (result, frames) =
        run_provider_configure(BTreeMap::from([("local.json".to_owned(), settings)]));

    assert!(result.is_ok(), "keyless Configure should enter the runtime");
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(frames.iter().any(|frame| {
        matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ProviderModelsDeclared(_))
        )
    }));
}

/// Proves invalid initial provider settings become one typed configuration
/// rejection and cannot publish partial models or cross the Ready boundary.
#[test]
fn invalid_provider_configure_emits_config_error_without_models_or_ready() {
    let valid = configured_chat_completions_settings("alpha", serde_json::json!({}));
    let invalid = configured_chat_completions_settings(
        "deepseek",
        serde_json::json!({
            "models": [{
                "id": "deepseek-chat",
                "local_summary_compaction": {
                    "context_window_tokens": 128000
                }
            }]
        }),
    );
    let (result, frames) = run_provider_configure(BTreeMap::from([
        ("alpha.json".to_owned(), valid),
        ("deepseek.json".to_owned(), invalid),
    ]));

    assert!(
        result.is_err(),
        "rejected startup must not enter the runtime loop"
    );
    let errors = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error.message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        errors,
        vec![
            "provider profile 'deepseek' has invalid credential-free settings: remove obsolete local_summary_compaction.context_window_tokens; model context_window is used"
        ]
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(!frames.iter().any(|frame| {
        matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ProviderModelsDeclared(_))
        )
    }));
}

/// Proves configuration diagnostics expose only the validated provider identity
/// and closed reason, never malformed settings values or path-like input.
#[test]
fn invalid_provider_configure_error_is_redacted() {
    let settings = configured_chat_completions_settings(
        "deepseek",
        serde_json::json!({
            "api_key_secret": "credential-value-sentinel",
            "base_url": "/private/provider/path-sentinel"
        }),
    );
    let (_, frames) =
        run_provider_configure(BTreeMap::from([("deepseek.json".to_owned(), settings)]));
    let diagnostic = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error.message.as_str()),
            _ => None,
        })
        .expect("ConfigError");

    assert!(diagnostic.contains("deepseek"));
    assert!(diagnostic.contains("credential fields are forbidden"));
    assert!(!diagnostic.contains("credential-value-sentinel"));
    assert!(!diagnostic.contains("/private/provider/path-sentinel"));
}

/// Proves a stored-credential route is excluded when initial Secret hydration
/// cannot obtain credential material before Ready.
#[test]
fn initial_secret_hydration_excludes_missing_credential_before_ready() {
    let settings = configured_chat_completions_settings("deepseek", serde_json::json!({}));
    let (result, frames) =
        run_provider_configure(BTreeMap::from([("deepseek.json".to_owned(), settings)]));

    result.expect("valid provider startup");
    assert!(frames.iter().any(|frame| {
        matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ProviderModelsDeclared(declaration)
                        if declaration.models.is_empty()
                )
        )
    }));
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
}

/// Proves malformed credential records exclude their routes while a locally
/// parseable record admits the route without making a remote-authentication
/// claim.
#[test]
fn credential_hydration_requires_local_usability_not_remote_authentication() {
    let provider = ProviderName::new("deepseek");
    let settings = configured_chat_completions_settings("deepseek", serde_json::json!({}));
    let configured = || {
        try_load_settings_profiles(vec![(provider.clone(), settings.clone())])
            .expect("valid credential-free settings")
    };

    let mut missing = configured();
    let missing_observations = hydrate_profile_credentials_with(&mut missing, |_| {
        Err(tau_client::ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            message: "fixture path is absent".to_owned(),
        })
    });
    assert!(models_for_profiles(&missing).is_empty());
    assert_eq!(
        missing_observations.get(&provider),
        Some(&CredentialObservation::Unavailable)
    );

    let mut malformed = configured();
    let malformed_observations = hydrate_profile_credentials_with(&mut malformed, |_| {
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: b"not-json".to_vec(),
        })
    });
    assert!(models_for_profiles(&malformed).is_empty());
    assert!(matches!(
        malformed_observations.get(&provider),
        Some(CredentialObservation::Contents(_))
    ));

    let empty_record =
        serde_json::to_vec(&credential_record::ApiKeyCredential::new(" \t".to_owned()))
            .expect("empty credential record");
    let mut empty = configured();
    hydrate_profile_credentials_with(&mut empty, |_| {
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: empty_record.clone(),
        })
    });
    assert!(
        models_for_profiles(&empty).is_empty(),
        "stored whitespace is not an implicit keyless credential"
    );

    let record = serde_json::to_vec(&credential_record::ApiKeyCredential::new(
        "locally-parseable-but-not-remotely-verified".to_owned(),
    ))
    .expect("credential record");
    let mut locally_usable = configured();
    hydrate_profile_credentials_with(&mut locally_usable, |_| {
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: record.clone(),
        })
    });
    assert!(
        models_for_profiles(&locally_usable)
            .iter()
            .any(|model| model.id.provider == provider),
        "local parsing admits the route without contacting the remote service"
    );
}

/// Proves a prompt-boundary lookup over a large validated snapshot clones and
/// Secret-reads only the selected profile, without exposing selected or
/// unselected secret canaries through debug formatting.
#[test]
fn large_profile_prompt_selection_clones_and_reads_only_selected_credential() {
    const PROFILE_COUNT: usize = 2_048;
    let target = ProviderName::new("provider-1024");
    let files = (0..PROFILE_COUNT)
        .map(|index| {
            let provider = ProviderName::new(format!("provider-{index:04}"));
            let settings = serde_json::to_vec(&serde_json::json!({
                "kind": "chat_completions",
                "base_url": format!("https://profile-{index}.example.invalid/v1"),
                "models": [{"id": format!("model-{index}")}],
                "credential": {
                    "kind": "api_key",
                    "identity": format!("{index:032x}")
                }
            }))
            .expect("large-profile settings");
            (provider, settings)
        })
        .collect::<Vec<_>>();
    let snapshot = try_load_settings_profiles(files).expect("validated large snapshot");
    let cloned_profiles = Cell::new(0_usize);
    let load = |selected: Option<&ProviderName>| {
        let profiles =
            selected.map_or_else(|| snapshot.clone(), |provider| snapshot.selected(provider));
        cloned_profiles.set(cloned_profiles.get() + profiles.providers.len());
        profiles
    };

    let mut selected = load(Some(&target));
    let selected_secret = "selected-api-key-privacy-canary";
    let credential_record = serde_json::to_vec(&credential_record::ApiKeyCredential::new(
        selected_secret.to_owned(),
    ))
    .expect("selected credential record");
    let mut secret_reads = 0_usize;
    let observations = hydrate_profile_credentials_with(&mut selected, |path| {
        secret_reads += 1;
        assert_eq!(
            path.as_str(),
            "providers/00000000000000000000000000000400/api-key.json"
        );
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: credential_record.clone(),
        })
    });

    assert_eq!(cloned_profiles.get(), 1);
    assert_eq!(secret_reads, 1);
    assert_eq!(selected.providers.len(), 1);
    assert_eq!(selected.credentials.len(), 1);
    assert_eq!(observations.len(), 1);
    let public_projection = format!("{observations:?} {:?}", models_for_profiles(&selected));
    assert!(!public_projection.contains(selected_secret));
    assert!(!public_projection.contains("profile-2047.example.invalid"));
}

/// Describes full-snapshot versus selected-profile clone and Secret-read
/// scaling as the validated profile count grows.
#[test]
#[ignore = "descriptive performance benchmark"]
fn benchmark_selected_profile_credential_resolution_scaling() {
    use std::hint::black_box;
    use std::time::Instant;

    eprintln!(
        "profiles,iterations,full_clone_ns,selected_clone_ns,full_secret_reads,selected_secret_reads"
    );
    for profile_count in [1_usize, 64, 1_024, 4_096] {
        let files = (0..profile_count)
            .map(|index| {
                let provider = ProviderName::new(format!("provider-{index:04}"));
                let settings = serde_json::to_vec(&serde_json::json!({
                    "kind": "chat_completions",
                    "models": [{"id": format!("model-{index}")}],
                    "credential": {
                        "kind": "api_key",
                        "identity": format!("{index:032x}")
                    }
                }))
                .expect("benchmark settings");
                (provider, settings)
            })
            .collect();
        let snapshot =
            try_load_settings_profiles(files).expect("validated benchmark profile snapshot");
        let target = ProviderName::new(format!("provider-{:04}", profile_count - 1));
        let iterations = 128;

        let full_started = Instant::now();
        for _ in 0..iterations {
            black_box(snapshot.clone());
        }
        let full_elapsed = full_started.elapsed();
        let selected_started = Instant::now();
        for _ in 0..iterations {
            black_box(snapshot.selected(&target));
        }
        let selected_elapsed = selected_started.elapsed();

        let record = serde_json::to_vec(&credential_record::ApiKeyCredential::new(
            "benchmark-key".to_owned(),
        ))
        .expect("benchmark credential");
        let mut full = snapshot.clone();
        let mut full_secret_reads = 0_usize;
        hydrate_profile_credentials_with(&mut full, |_| {
            full_secret_reads += 1;
            Ok(tau_proto::ExtensionDataValue::ReadFile {
                contents: record.clone(),
            })
        });
        let mut selected = snapshot.selected(&target);
        let mut selected_secret_reads = 0_usize;
        hydrate_profile_credentials_with(&mut selected, |_| {
            selected_secret_reads += 1;
            Ok(tau_proto::ExtensionDataValue::ReadFile {
                contents: record.clone(),
            })
        });

        eprintln!(
            "{profile_count},{iterations},{},{},{full_secret_reads},{selected_secret_reads}",
            full_elapsed.as_nanos(),
            selected_elapsed.as_nanos()
        );
    }
}

/// Proves a changed usable credential generation republishes a replacement even
/// when the locally usable model list remains identical.
#[test]
fn credential_replacement_requires_replacement_declaration() {
    let provider = ProviderName::new("deepseek");
    let models = Vec::<ProviderModelInfo>::new();
    let previous = BTreeMap::from([(
        provider.clone(),
        CredentialObservation::Contents(blake3::hash(b"credential-a")),
    )]);
    let replacement = BTreeMap::from([(
        provider,
        CredentialObservation::Contents(blake3::hash(b"credential-b")),
    )]);

    assert!(declaration_needs_publication(
        Some(&models),
        Some(&previous),
        &models,
        &replacement,
    ));
    assert!(!declaration_needs_publication(
        Some(&models),
        Some(&replacement),
        &models,
        &replacement,
    ));
}

/// Proves a selected ChatGPT identity rotation rebuilds a complete declaration
/// without dropping a sibling provider's models.
#[test]
fn selected_identity_rotation_declaration_preserves_sibling_models() {
    let chatgpt = ProviderName::new("chatgpt");
    let sibling = ProviderName::new("sibling");
    let profiles = try_load_settings_profiles(vec![
        (
            chatgpt.clone(),
            serde_json::to_vec(&serde_json::json!({
                "kind": "chatgpt",
                "credential": {
                    "kind": "oauth",
                    "identity": "0123456789abcdef0123456789abcdef"
                }
            }))
            .expect("ChatGPT settings"),
        ),
        (
            sibling.clone(),
            serde_json::to_vec(&serde_json::json!({
                "kind": "chat_completions",
                "models": [{"id": "sibling-model"}],
                "credential": {"kind": "none"}
            }))
            .expect("sibling settings"),
        ),
    ])
    .expect("two-provider settings");
    let complete = models_for_profiles(&profiles);
    let selected = profiles.selected(&chatgpt);

    let replacement = replace_provider_models(&complete, &chatgpt, &selected);

    assert_eq!(replacement, complete);
    assert!(
        replacement.iter().any(|model| model.id.provider == sibling),
        "a selected identity replacement must retain sibling routes"
    );
}

/// Proves the centralized post-OAuth-resolution boundary always performs the
/// authoritative rehydration that detects a refresh or adopted CAS winner.
#[test]
fn oauth_resolution_observes_authoritative_replacement_generation() {
    let provider = ProviderName::new("chatgpt");
    let models = Vec::<ProviderModelInfo>::new();
    let previous = BTreeMap::from([(
        provider.clone(),
        CredentialObservation::Contents(blake3::hash(b"pre-refresh")),
    )]);
    let authoritative = BTreeMap::from([(
        provider,
        CredentialObservation::Contents(blake3::hash(b"cas-winner")),
    )]);
    let mut published = false;

    observe_oauth_resolution_with(true, || {
        published =
            declaration_needs_publication(Some(&models), Some(&previous), &models, &authoritative);
        Ok(())
    })
    .expect("post-resolution observation");

    assert!(published, "authoritative OAuth generation republishes");
}

/// Proves rotating one alias does not clear compact-negative evidence still
/// referenced by an unchanged provider with the same inference identity.
#[test]
fn credential_rotation_retains_shared_alias_compact_negative_evidence() {
    let changed = ProviderName::new("changed");
    let unchanged = ProviderName::new("unchanged");
    let identity = InferenceProfileIdentity::from_test_value(7);
    let previous = BTreeMap::from([
        (
            changed.clone(),
            CredentialObservation::Contents(blake3::hash(b"old")),
        ),
        (
            unchanged.clone(),
            CredentialObservation::Contents(blake3::hash(b"shared")),
        ),
    ]);
    let current = BTreeMap::from([
        (
            changed.clone(),
            CredentialObservation::Contents(blake3::hash(b"new")),
        ),
        (
            unchanged.clone(),
            CredentialObservation::Contents(blake3::hash(b"shared")),
        ),
    ]);
    let mut identities = HashMap::from([(changed, identity), (unchanged, identity)]);
    let mut unavailable = HashSet::from([identity]);

    let superseded = reconcile_compact_state_after_credential_changes(
        &previous,
        &current,
        &mut identities,
        &mut unavailable,
    );

    assert!(superseded.is_empty());
    assert!(unavailable.contains(&identity));
}

/// Rotating one alias while a shared identity is still probing does not retire
/// the probe owned by the unchanged alias.
#[test]
fn credential_rotation_retains_shared_alias_compact_probe() {
    let changed = ProviderName::new("changed");
    let unchanged = ProviderName::new("unchanged");
    let identity = InferenceProfileIdentity::from_test_value(9);
    let previous = BTreeMap::from([
        (
            changed.clone(),
            CredentialObservation::Contents(blake3::hash(b"old")),
        ),
        (
            unchanged.clone(),
            CredentialObservation::Contents(blake3::hash(b"shared")),
        ),
    ]);
    let current = BTreeMap::from([
        (
            changed.clone(),
            CredentialObservation::Contents(blake3::hash(b"new")),
        ),
        (
            unchanged.clone(),
            CredentialObservation::Contents(blake3::hash(b"shared")),
        ),
    ]);
    let mut identities = HashMap::from([(changed, identity), (unchanged, identity)]);
    let mut unavailable = HashSet::new();

    let superseded = reconcile_compact_state_after_credential_changes(
        &previous,
        &current,
        &mut identities,
        &mut unavailable,
    );

    assert!(superseded.is_empty());
    assert_eq!(
        identities
            .values()
            .filter(|value| **value == identity)
            .count(),
        1
    );
}

/// Rotating the last provider alias for a negative inference identity retires
/// that superseded generation from both extension and Codex runtime ownership.
#[test]
fn credential_rotation_evicts_unaliased_compact_negative_evidence() {
    let provider = ProviderName::new("chatgpt");
    let identity = InferenceProfileIdentity::from_test_value(8);
    let previous = BTreeMap::from([(
        provider.clone(),
        CredentialObservation::Contents(blake3::hash(b"old")),
    )]);
    let current = BTreeMap::from([(
        provider.clone(),
        CredentialObservation::Contents(blake3::hash(b"new")),
    )]);
    let mut identities = HashMap::from([(provider, identity)]);
    let mut unavailable = HashSet::from([identity]);

    let superseded = reconcile_compact_state_after_credential_changes(
        &previous,
        &current,
        &mut identities,
        &mut unavailable,
    );

    assert_eq!(superseded, vec![identity]);
    assert!(!unavailable.contains(&identity));
}

/// A downgrade message queued by a superseded worker does not repopulate the
/// extension's negative identity cache.
#[test]
fn late_superseded_compact_downgrade_is_not_retained() {
    let provider = ProviderName::new("rotated");
    let stale = InferenceProfileIdentity::from_test_value(10);
    let current = InferenceProfileIdentity::from_test_value(11);
    let identities = HashMap::from([(provider, current)]);

    assert!(!compact_negative_identity_is_owned(stale, &identities));
}

/// A late downgrade from a rotated origin alias remains authoritative while a
/// sibling alias still owns the rejected inference identity.
#[test]
fn late_rotated_alias_downgrade_is_retained_for_shared_identity() {
    let rotated = ProviderName::new("rotated");
    let sibling = ProviderName::new("sibling");
    let rejected = InferenceProfileIdentity::from_test_value(12);
    let current = InferenceProfileIdentity::from_test_value(13);
    let identities = HashMap::from([(rotated, current), (sibling, rejected)]);

    assert!(compact_negative_identity_is_owned(rejected, &identities));
}

/// Cloneable in-memory sink used to inspect structured tracing output.
#[derive(Clone, Default)]
pub(super) struct SharedTraceWriter {
    /// Bytes written by the temporary tracing subscriber.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Wakes tests waiting for a causal trace-publication cut.
    changed: Arc<Condvar>,
}

impl SharedTraceWriter {
    /// Returns the trace bytes captured by this test sink.
    pub(super) fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("trace writer lock").clone()
    }

    /// Waits until the trace contains `expected` exact byte-string occurrences.
    fn wait_for_occurrences(
        &self,
        needle: &[u8],
        expected: usize,
        timeout: path_std_time::Duration,
    ) -> bool {
        let bytes = self.bytes.lock().expect("trace writer lock");
        let (bytes, _) = self
            .changed
            .wait_timeout_while(bytes, timeout, |bytes| {
                bytes
                    .windows(needle.len())
                    .filter(|window| *window == needle)
                    .count()
                    < expected
            })
            .expect("trace writer wait");
        expected
            <= bytes
                .windows(needle.len())
                .filter(|window| *window == needle)
                .count()
    }
}

impl Write for SharedTraceWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace writer lock")
            .extend_from_slice(buffer);
        self.changed.notify_all();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every provider retry class must map to its stable provider retry category.
#[test]
fn retry_classes_map_to_provider_categories() {
    for (class, expected) in [
        (
            RetryClass::Transport,
            tau_proto::ProviderRetryCategory::Transport,
        ),
        (
            RetryClass::Overload,
            tau_proto::ProviderRetryCategory::Overload,
        ),
        (
            RetryClass::Throttle,
            tau_proto::ProviderRetryCategory::Throttle,
        ),
        (
            RetryClass::UsageWindow,
            tau_proto::ProviderRetryCategory::UsageWindow,
        ),
        (
            RetryClass::Account,
            tau_proto::ProviderRetryCategory::Account,
        ),
        (RetryClass::Auth, tau_proto::ProviderRetryCategory::Auth),
        (
            RetryClass::Unknown,
            tau_proto::ProviderRetryCategory::Unknown,
        ),
    ] {
        assert_eq!(retry_class_provider_category(class), expected);
    }
}

/// Retry telemetry conversion must saturate rather than wrap at wire bounds.
#[test]
fn retry_status_numeric_fields_saturate_to_wire_bounds() {
    assert_eq!(saturating_retry_attempt(u64::MAX), u32::MAX);
    assert_eq!(
        saturating_retry_delay(Duration::from_secs(u64::MAX)),
        u32::MAX
    );
    assert_eq!(saturating_retry_attempt(7), 7);
    assert_eq!(saturating_retry_delay(Duration::from_secs(8)), 8);
}

struct RecordingRetrySleeper;

struct NoopAbortWaker;

impl TurnAbortWaker for NoopAbortWaker {}

impl TurnAbort for RecordingRetrySleeper {
    fn is_aborted(&mut self) -> bool {
        false
    }

    fn register_waker(
        &mut self,
        _waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(NoopAbortWaker)
    }
}

fn model_ids(models: &[ProviderModelInfo]) -> Vec<String> {
    models.iter().map(|model| model.id.to_string()).collect()
}

/// Server-side compaction is a durable output item and therefore ends a turn
/// normally rather than using a special provider lifecycle stop reason.
#[test]
fn compaction_output_finishes_as_normal_end_turn() {
    let output_items = [tau_proto::ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"compaction"}"#)
            .expect("valid compaction item"),
    )];

    assert_eq!(
        stop_reason_from_output_items(&output_items),
        tau_proto::ProviderStopReason::EndTurn
    );
}

/// Tool calls returned beside compaction must still own the stop reason so the
/// harness executes them instead of treating the turn as normally complete.
#[test]
fn compaction_with_tool_calls_still_requests_tools() {
    let output_items = [
        tau_proto::ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"compaction"}"#)
                .expect("valid compaction item"),
        ),
        tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: "call-compact-tool".into(),
            name: tau_proto::ToolName::new("echo"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Null,
            raw_arguments_json: None,
            responses_envelope: None,
        }),
    ];

    assert_eq!(
        stop_reason_from_output_items(&output_items),
        tau_proto::ProviderStopReason::ToolCalls
    );
}

/// Runtime and provider setup errors must stay outside replayable assistant
/// output items while still producing an error terminal.
#[test]
fn synthetic_provider_error_is_not_output_item() {
    let finished = simple_finished(
        "sp-error"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        tau_proto::AgentId::parse("agent").expect("valid test agent id"),
        tau_proto::PromptOriginator::User,
        "no model specified",
    );

    assert!(finished.output_items.is_empty());
    assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(finished.error.as_deref(), Some("no model specified"));
}

/// Proves malformed login invocation fails before profile lookup, secret
/// prompts, or OAuth network work.
#[test]
fn login_subcommand_requires_exactly_one_existing_profile_name() {
    let args = vec!["login".to_owned()];

    let error = run_provider_cli(&args).expect_err("missing profile name");

    assert!(
        error
            .to_string()
            .contains("provider login requires exactly one NAME"),
        "{error}"
    );
}

/// Canonical provider-kind tokens remain the only accepted non-interactive
/// spellings, so setup never preserves human picker labels as aliases.
#[test]
fn provider_kind_catalog_has_exact_canonical_tokens() {
    assert_eq!(
        PROVIDER_KINDS
            .iter()
            .map(|descriptor| descriptor.token)
            .collect::<Vec<_>>(),
        ["chatgpt", "chat-completions", "responses", "openrouter"]
    );
    assert!(
        !PROVIDER_KINDS
            .iter()
            .any(|descriptor| descriptor.token == "completions API")
    );
}

/// ChatGPT's route compatibility flag is optional, omits its standard default,
/// and round-trips only when explicitly enabled.
#[test]
fn chatgpt_profile_responses_lite_compatibility_serde_contract() {
    let missing: BuiltinProviderProfile = serde_json::from_value(serde_json::json!({
        "kind": "chatgpt",
        "auth": {}
    }))
    .expect("legacy profile");
    let BuiltinProviderProfile::Chatgpt(missing) = missing else {
        panic!("chatgpt profile");
    };
    assert!(!missing.responses_lite_compatibility);

    let standard = serde_json::to_value(BuiltinProviderProfile::Chatgpt(ChatGptProfile::default()))
        .expect("standard profile");
    assert!(standard.get("responses_lite_compatibility").is_none());

    let lite = BuiltinProviderProfile::Chatgpt(ChatGptProfile {
        auth: OpenAiAuth::default(),
        responses_lite_compatibility: true,
        cache_diagnostics: Default::default(),
    });
    let value = serde_json::to_value(&lite).expect("lite profile");
    assert_eq!(value["responses_lite_compatibility"], true);
    assert!(matches!(
        serde_json::from_value::<BuiltinProviderProfile>(value).expect("round trip"),
        BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            responses_lite_compatibility: true,
            ..
        })
    ));
}

/// Interactive ChatGPT setup must remain standard-by-default unless the user
/// explicitly confirms the compatibility prompt.
#[test]
fn chatgpt_setup_defaults_responses_lite_compatibility_to_no() {
    assert!(!std::hint::black_box(DEFAULT_RESPONSES_LITE_COMPATIBILITY));
}

/// Metadata selection is a closed default-on profile field, frozen separately
/// from credential reload and never coupled to existing exact capture policy.
#[test]
fn chatgpt_cache_diagnostics_profile_selection_is_closed_and_default_on() {
    use tau_provider::cache_diagnostic::CacheDiagnostics;
    let default: ChatGptProfile = serde_json::from_str("{}").expect("default profile");
    assert_eq!(default.cache_diagnostics, CacheDiagnostics::Metadata);
    assert!(
        serde_json::to_value(default)
            .expect("profile serialization")
            .get("cache_diagnostics")
            .is_none()
    );
    let off: ChatGptProfile =
        serde_json::from_str(r#"{"cache_diagnostics":"off"}"#).expect("metadata opt-out");
    assert_eq!(off.cache_diagnostics, CacheDiagnostics::Off);
    assert!(serde_json::from_str::<ChatGptProfile>(r#"{"cache_diagnostics":"raw"}"#).is_err());
    assert!(serde_json::from_str::<ChatGptProfile>(r#"{"cache_diagnostics":false}"#).is_err());
}

/// Provider setup may recommend WebSocket only for OpenAI's exact official
/// Responses base URL; lookalike and compatible endpoints must remain SSE.
#[test]
fn responses_transport_recommendation_is_endpoint_exact() {
    assert_eq!(
        recommended_responses_transport("https://api.openai.com/v1"),
        ResponsesTransport::Websocket
    );
    assert_eq!(
        recommended_responses_transport("https://api.openai.com/v1/"),
        ResponsesTransport::Websocket
    );
    for endpoint in [
        "http://api.openai.com/v1",
        "https://api.openai.com/v1/responses",
        "https://compatible.example/v1",
    ] {
        assert_eq!(
            recommended_responses_transport(endpoint),
            ResponsesTransport::Sse
        );
    }
}

/// OAuth refresh replaces only credentials, so serializing and reloading the
/// saved full profile preserves the sibling Responses compatibility setting.
#[test]
fn oauth_auth_replacement_preserves_responses_lite_compatibility() {
    let mut profile = ChatGptProfile {
        auth: OpenAiAuth::default(),
        responses_lite_compatibility: true,
        cache_diagnostics: Default::default(),
    };
    profile.replace_auth(OpenAiAuth {
        access_token: "fresh".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: 42,
        account_id: Some("account".to_owned()),
    });
    let saved = serde_json::to_value(BuiltinProviderProfile::Chatgpt(profile))
        .expect("serialize refreshed profile");
    let reloaded: BuiltinProviderProfile =
        serde_json::from_value(saved).expect("reload refreshed profile");

    assert!(matches!(
        reloaded,
        BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            responses_lite_compatibility: true,
            auth: OpenAiAuth { access_token, .. },
            ..
        }) if access_token == "fresh"
    ));
}

fn oauth_test_jwt(account_id: &str) -> String {
    let payload = match account_id {
        "account-a" => {
            "eyJleHAiOjE4NDQ2NzQ0MDczNzA5NTUxLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1hIn19"
        }
        "account-b" => {
            "eyJleHAiOjE4NDQ2NzQ0MDczNzA5NTUxLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1iIn19"
        }
        _ => panic!("unsupported fixture account"),
    };
    format!("header.{payload}.signature")
}

/// Rotation preserves omitted credentials but refuses a replacement access
/// token whose account claim crosses the pinned ChatGPT identity.
#[test]
fn oauth_refresh_merge_preserves_omissions_and_rejects_identity_change() {
    let current = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh-a".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account-a".to_owned()),
    };
    let preserved = merge_chatgpt_refresh(
        &current,
        path_tau_provider_codex_oauth::OAuthTokenRefresh {
            access_token: None,
            refresh_token: None,
            expires_at_ms: None,
            account_id: None,
        },
    )
    .expect("omitted replacements preserve the pinned generation");
    assert_eq!(preserved, current);
    let anchored_without_claim = OpenAiAuth {
        access_token: "opaque-access".to_owned(),
        ..current.clone()
    };
    let preserved = merge_chatgpt_refresh(
        &anchored_without_claim,
        path_tau_provider_codex_oauth::OAuthTokenRefresh {
            access_token: None,
            refresh_token: None,
            expires_at_ms: None,
            account_id: None,
        },
    )
    .expect("omitted access preserves stored account anchor");
    assert_eq!(preserved.account_id.as_deref(), Some("account-a"));

    let mismatch = merge_chatgpt_refresh(
        &current,
        path_tau_provider_codex_oauth::OAuthTokenRefresh {
            access_token: Some(oauth_test_jwt("account-b")),
            refresh_token: Some("refresh-b".to_owned()),
            expires_at_ms: Some(u64::MAX),
            account_id: Some("account-b".to_owned()),
        },
    );
    assert!(matches!(
        mismatch,
        Err(RefreshCredentialsError::IdentityMismatch)
    ));
}

/// A losing CAS may adopt only a concurrently published credential from the
/// pinned account, and forced recovery still requires a new access token.
#[test]
fn oauth_cas_race_adoption_preserves_identity_and_replaces_rejected_access() {
    let current = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh-a".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account-a".to_owned()),
    };
    let mut winner = current.clone();
    winner.access_token = format!("{}-rotated", oauth_test_jwt("account-a"));
    winner.refresh_token = "refresh-winner".to_owned();
    validate_authoritative_rotation(&current, &winner, true)
        .expect("same-account winner with replacement access token");

    assert!(matches!(
        validate_authoritative_rotation(&current, &current, true),
        Err(RefreshCredentialsError::RejectedGeneration)
    ));
    let mut crossed = winner;
    crossed.account_id = Some("account-b".to_owned());
    crossed.access_token = oauth_test_jwt("account-b");
    assert!(matches!(
        validate_authoritative_rotation(&current, &crossed, true),
        Err(RefreshCredentialsError::IdentityMismatch)
    ));
}

/// Canonical unauthorized recovery grants one forced refresh to the exact
/// rejected inference generation and does not reset on repeated 401 reports.
#[test]
fn oauth_unauthorized_recovery_is_exact_generation_and_once_only() {
    let provider = ProviderName::new("chatgpt");
    let mut cache = OAuthRefreshRejectionCache::default();
    let first = BackendProfileIdentity::from_test_value(41);
    let different = BackendProfileIdentity::from_test_value(40);
    let second = BackendProfileIdentity::from_test_value(42);
    cache.record_unauthorized(provider.clone(), first);
    assert!(!cache.take_unauthorized(&provider, different));
    assert!(cache.take_unauthorized(&provider, first));
    assert!(cache.unauthorized_exhausted(&provider, first));
    cache.record_unauthorized(provider.clone(), first);
    assert!(!cache.take_unauthorized(&provider, first));
    cache.record_unauthorized(provider.clone(), second);
    assert!(cache.take_unauthorized(&provider, second));
    cache.record_unauthorized(provider.clone(), first);
    cache.record_unauthorized(provider.clone(), second);
    assert!(cache.unauthorized_exhausted(&provider, second));
    assert!(!cache.take_unauthorized(&provider, second));
    cache.clear_refresh_rejection(&provider);
    cache.record_unauthorized(provider.clone(), first);
    assert!(cache.unauthorized_exhausted(&provider, first));
    assert!(!cache.take_unauthorized(&provider, first));
    cache.clear(&provider);
    cache.record_unauthorized(provider.clone(), first);
    assert!(cache.unauthorized_exhausted(&provider, first));
}

/// A canonical 401 forces refresh while the token is locally valid, and a
/// failed recovery makes that exact generation unavailable without a second
/// endpoint attempt.
#[test]
fn oauth_unauthorized_forces_once_then_suppresses_rejected_generation() {
    let provider = ProviderName::new("chatgpt-fedi");
    let credential_reference = oauth_test_credential_reference();
    let model = ModelId::new(provider.clone(), ModelName::new("gpt-5.4"));
    let original = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh-a".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account-a".to_owned()),
    };
    let config = tau_provider_codex::resolved_config_for_provider_model(
        &provider,
        &model.model,
        tau_provider_codex::ResolvedCredentials::new(
            original.access_token.clone(),
            original.account_id.clone(),
        ),
        CodexMode::Standard,
    );
    let identity = backend_profile_identity(&PromptBackend::Responses(config))
        .expect("ChatGPT backend identity");
    let mut cache = OAuthRefreshRejectionCache::default();
    cache.record_unauthorized(provider.clone(), identity);
    let attempts = Cell::new(0);
    let rejection = path_tau_provider_codex_oauth::OAuthError::from_http_response(502, "{}");

    let mut auth = original.clone();
    let first = resolve_chatgpt_backend_with_refresh(
        &model,
        &provider,
        &credential_reference,
        &mut auth,
        CodexMode::Standard,
        &mut cache,
        |_, reference, _, _, force| {
            assert!(force);
            assert_eq!(
                reference.path().as_str(),
                "providers/0123456789abcdef0123456789abcdef/oauth.json"
            );
            attempts.set(attempts.get() + 1);
            Err(RefreshCredentialsError::OAuth {
                credentials: Box::new(original.clone()),
                error: rejection.clone(),
            })
        },
    );
    assert!(first.is_none());
    assert_eq!(attempts.get(), 1);

    auth = original;
    let second = resolve_chatgpt_backend_with_refresh(
        &model,
        &provider,
        &credential_reference,
        &mut auth,
        CodexMode::Standard,
        &mut cache,
        |_, _, _, _, _| panic!("rejected generation must remain suppressed"),
    );
    assert!(second.is_none());
    assert_eq!(attempts.get(), 1);
}

/// Ensures the refresh read, CAS publication, and losing-CAS reload all use
/// the parsed credential identity rather than the provider namespace.
#[test]
fn oauth_refresh_cas_and_reload_follow_credential_identity() {
    let provider = ProviderName::new("chatgpt-fedi");
    let credential_reference = oauth_test_credential_reference();
    let current = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh-current".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account-a".to_owned()),
    };
    let mut winner = current.clone();
    winner.access_token = format!("{}-winner", oauth_test_jwt("account-a"));
    winner.refresh_token = "refresh-winner".to_owned();
    let current_record = serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
        current.clone(),
    ))
    .expect("encode current credential");
    let winner_record = serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
        winner.clone(),
    ))
    .expect("encode winning credential");
    let mut results = VecDeque::from([
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: current_record,
        }),
        Err(OAuthCredentialStorageError::GenerationMismatch),
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: winner_record,
        }),
    ]);
    let mut operations = Vec::new();
    let mut rejections = OAuthRefreshRejectionCache::default();

    let refreshed = refresh_chatgpt_credentials_with(
        &provider,
        &credential_reference,
        CodexMode::Standard,
        &mut rejections,
        true,
        |operation| {
            operations.push(operation);
            results.pop_front().expect("expected Secret RPC operation")
        },
        |refresh_token| {
            assert_eq!(refresh_token, "refresh-current");
            Ok(path_tau_provider_codex_oauth::OAuthTokenRefresh {
                access_token: Some(format!("{}-attempt", oauth_test_jwt("account-a"))),
                refresh_token: Some("refresh-attempt".to_owned()),
                expires_at_ms: Some(u64::MAX),
                account_id: Some("account-a".to_owned()),
            })
        },
    )
    .expect("losing CAS adopts same-account winner");

    assert_eq!(refreshed, winner);
    assert!(results.is_empty(), "all expected Secret RPC operations ran");
    let paths = operations
        .into_iter()
        .map(|operation| match operation {
            tau_proto::ExtensionDataRequestOp::ReadFile { path }
            | tau_proto::ExtensionDataRequestOp::CompareAndSwapFile { path, .. } => path,
            _ => panic!("unexpected OAuth credential operation"),
        })
        .map(|path| path.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "providers/0123456789abcdef0123456789abcdef/oauth.json",
            "providers/0123456789abcdef0123456789abcdef/oauth.json",
            "providers/0123456789abcdef0123456789abcdef/oauth.json",
        ],
        "initial read, CAS, and reload retain the parsed credential identity"
    );
}

/// Forced recovery without a refresh credential cannot return the exact
/// rejected access token as a usable backend.
#[test]
fn oauth_unauthorized_with_empty_refresh_token_fails_closed() {
    let current = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: String::new(),
        expires_at_ms: u64::MAX,
        account_id: Some("account-a".to_owned()),
    };
    assert!(matches!(
        refresh_required(&current, true),
        Err(RefreshCredentialsError::RejectedGeneration)
    ));
}

/// Automatic retry can reload token rotation only within the pinned account.
#[test]
fn automatic_retry_refuses_cross_account_backend_rotation() {
    let model = ModelName::new("gpt-5.4");
    let config = |account: &str, token_suffix: &str| {
        PromptBackend::Responses(tau_provider_codex::resolved_config_for_provider_model(
            &ProviderName::new("chatgpt"),
            &model,
            tau_provider_codex::ResolvedCredentials::new(
                format!("{}-{token_suffix}", oauth_test_jwt(account)),
                Some(account.to_owned()),
            ),
            CodexMode::Standard,
        ))
    };
    let PromptBackend::Responses(anchor) = config("account-a", "initial") else {
        unreachable!()
    };
    let anchor = anchor.chatgpt_retry_identity();
    assert!(
        std::mem::size_of_val(&anchor) <= 40,
        "retry pin retains one closed digest rather than bearer-bearing config"
    );
    assert!(automatic_retry_identity_matches(
        Some(&anchor),
        &config("account-a", "rotated")
    ));
    assert!(automatic_retry_identity_matches(
        Some(&anchor),
        &PromptBackend::Unavailable {
            login_required: None,
        }
    ));
    assert!(!automatic_retry_identity_matches(
        Some(&anchor),
        &config("account-b", "initial")
    ));
    let missing = tau_provider_codex::resolved_config_for_provider_model(
        &ProviderName::new("chatgpt"),
        &model,
        tau_provider_codex::ResolvedCredentials::new("credential-content-canary".to_owned(), None),
        CodexMode::Standard,
    )
    .chatgpt_retry_identity();
    assert!(
        !automatic_retry_identity_matches(Some(&missing), &config("account-a", "after-missing")),
        "missing initial account identity must not authorize account adoption"
    );
}

/// CAS adoption requires equal non-empty identity anchors.
#[test]
fn oauth_cas_adoption_rejects_missing_and_blank_identity() {
    for account_id in [None, Some(String::new()), Some(" ".to_owned())] {
        let current = OpenAiAuth {
            access_token: "token-without-claims".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_ms: u64::MAX,
            account_id: account_id.clone(),
        };
        let mut winner = current.clone();
        winner.access_token.push_str("-new");
        assert!(matches!(
            validate_authoritative_rotation(&current, &winner, true),
            Err(RefreshCredentialsError::IdentityMismatch)
        ));
    }
}

/// Startup quota initialization must resolve one model per ChatGPT profile, so
/// one rejected refresh cannot be amplified by every published model. The
/// typed trace projection excludes arbitrary provider fields and preserves
/// provider attribution.
#[test]
fn startup_quota_initialization_resolves_once_per_provider() {
    let first = ProviderName::new("first");
    let second = ProviderName::new("second");
    let expired = OpenAiAuth {
        access_token: "expired".to_owned(),
        refresh_token: "reused".to_owned(),
        expires_at_ms: now_ms().saturating_sub(1),
        account_id: None,
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: BTreeMap::from([
            (
                first.clone(),
                ProviderCredential::Stored(oauth_test_credential_reference()),
            ),
            (
                second.clone(),
                ProviderCredential::Stored(oauth_test_credential_reference()),
            ),
        ]),
        providers: BTreeMap::from([
            (
                first.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile {
                    auth: expired.clone(),
                    responses_lite_compatibility: false,
                    cache_diagnostics: Default::default(),
                }),
            ),
            (
                second.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile {
                    auth: expired,
                    responses_lite_compatibility: false,
                    cache_diagnostics: Default::default(),
                }),
            ),
            (
                ProviderName::new("router"),
                BuiltinProviderProfile::OpenRouter(OpenRouterProfile::default()),
            ),
        ]),
        missing_logins: Default::default(),
    };

    let published = models_for_profiles(&profiles);
    let mut attempts = Vec::new();
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    let reflected_secret = "oauth-reflected-secret";
    let rejection_body = serde_json::json!({
        "error": {
            "code": reflected_secret,
            "message": format!("reflected {reflected_secret}"),
        }
    })
    .to_string();
    let rejection =
        path_tau_provider_codex_oauth::OAuthError::from_http_response(400, &rejection_body);
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();
    let resolved = tracing::subscriber::with_default(subscriber, || {
        profiles.resolve_initial_quota_backends(|model, profiles| {
            let BuiltinProviderProfile::Chatgpt(profile) = profiles
                .providers
                .get_mut(&model.provider)
                .expect("selected ChatGPT profile")
            else {
                panic!("quota initialization selected non-ChatGPT profile");
            };
            let mode = profile.responses_mode();
            let authoritative = profile.auth.clone();
            resolve_chatgpt_backend_with_refresh(
                model,
                &model.provider,
                &oauth_test_credential_reference(),
                &mut profile.auth,
                mode,
                &mut refresh_rejections,
                |provider, _, _, _, _| {
                    attempts.push(provider.clone());
                    Err(RefreshCredentialsError::OAuth {
                        credentials: Box::new(authoritative),
                        error: rejection.clone(),
                    })
                },
            )
        })
    });
    let trace = String::from_utf8(trace.bytes()).expect("UTF-8 trace output");

    assert!(resolved.is_empty(), "expired credentials stay unavailable");
    assert!(published.len() > attempts.len());
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts.iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([first.clone(), second.clone()])
    );
    assert_eq!(
        trace
            .matches("failed to refresh ChatGPT credentials")
            .count(),
        2
    );
    assert!(trace.contains(&format!("provider={first}")));
    assert!(trace.contains(&format!("provider={second}")));
    assert!(!trace.contains("provider=router"));
    assert!(trace.contains("HTTP 400"));
    assert!(!trace.contains(reflected_secret));
    for warning in trace.lines().filter(|line| line.contains(" WARN ")) {
        assert!(!warning.contains("provider="));
        assert!(!warning.contains("HTTP 400"));
    }

    for profile in profiles.providers.values_mut() {
        if let BuiltinProviderProfile::Chatgpt(profile) = profile {
            profile.auth.expires_at_ms = u64::MAX;
        }
    }
    let mut successful_rejections = OAuthRefreshRejectionCache::default();
    let successful = profiles.resolve_initial_quota_backends(|model, profiles| {
        resolve_responses_backend(
            model,
            profiles,
            &mut successful_rejections,
            &test_network_policy(),
            None,
        )
    });
    assert_eq!(successful.len(), 2);
}

/// A permanent provider rejection is attempted once for an exact on-disk
/// credential and Responses-mode generation. Credential or mode replacement
/// permits a new attempt, while a valid replacement clears stale suppression
/// without calling the endpoint.
#[test]
fn refresh_failure_falls_back_only_to_still_valid_access_token() {
    let provider = ProviderName::new("chatgpt-fedi");
    let credential_reference = oauth_test_credential_reference();
    let model = ModelId::new(provider.clone(), ModelName::new("gpt-5.4"));
    let rejection = path_tau_provider_codex_oauth::OAuthError::from_http_response(
        400,
        r#"{"error":{"code":"refresh_token_reused"}}"#,
    );
    let preemptive_refreshes = Cell::new(0);

    for (expires_at_ms, refresh_token, expected_available) in [
        (now_ms().saturating_sub(1), "refresh", false),
        (now_ms().saturating_sub(1), "", false),
        (now_ms().saturating_add(60_000), "refresh", true),
    ] {
        let mut auth = OpenAiAuth {
            access_token: "access".to_owned(),
            refresh_token: refresh_token.to_owned(),
            expires_at_ms,
            account_id: None,
        };
        let mut cache = OAuthRefreshRejectionCache::default();
        let authoritative = auth.clone();
        let config = resolve_chatgpt_backend_with_refresh(
            &model,
            &provider,
            &credential_reference,
            &mut auth,
            CodexMode::Standard,
            &mut cache,
            |requested_provider, reference, _, _, force| {
                assert_eq!(requested_provider, &provider);
                assert_eq!(
                    reference.path().as_str(),
                    "providers/0123456789abcdef0123456789abcdef/oauth.json"
                );
                assert!(!force, "expiry-triggered refresh is not forced recovery");
                preemptive_refreshes.set(preemptive_refreshes.get() + 1);
                Err(RefreshCredentialsError::OAuth {
                    credentials: Box::new(authoritative),
                    error: rejection.clone(),
                })
            },
        );

        assert_eq!(config.is_some(), expected_available);
    }
    assert_eq!(
        preemptive_refreshes.get(),
        2,
        "expired and near-expiry credentials both preemptively refresh"
    );
}

/// Startup-selected modes independently control same-process publication and
/// overwrite later disk edits until restart.
#[test]
fn chatgpt_profile_modes_are_independent_and_startup_stable() {
    let standard = ProviderName::new("standard");
    let lite = ProviderName::new("lite");
    let mut startup = BuiltinProviderProfiles {
        credentials: Default::default(),
        missing_logins: Default::default(),
        providers: BTreeMap::from([
            (
                standard.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile::default()),
            ),
            (
                lite.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile {
                    auth: OpenAiAuth::default(),
                    responses_lite_compatibility: true,
                    cache_diagnostics: Default::default(),
                }),
            ),
        ]),
    };
    let modes = startup.startup_responses_modes();
    let models = models_for_profiles(&startup);
    let standard_sol = models
        .iter()
        .find(|model| model.id.provider == standard && model.id.model.as_str() == "gpt-5.6-sol")
        .expect("standard Sol");
    let lite_sol = models
        .iter()
        .find(|model| model.id.provider == lite && model.id.model.as_str() == "gpt-5.6-sol")
        .expect("lite Sol");
    assert!(standard_sol.supports_parallel_tool_calls);
    assert!(!lite_sol.supports_parallel_tool_calls);
    assert!(!standard_sol.supports_compaction && standard_sol.supports_standalone_compaction);
    assert!(!lite_sol.supports_compaction && lite_sol.supports_standalone_compaction);

    for profile in startup.providers.values_mut() {
        let BuiltinProviderProfile::Chatgpt(profile) = profile else {
            unreachable!()
        };
        profile.responses_lite_compatibility = !profile.responses_lite_compatibility;
    }
    startup.apply_startup_responses_modes(&modes);
    assert!(matches!(
        startup.providers.get(&standard),
        Some(BuiltinProviderProfile::Chatgpt(profile))
            if !profile.responses_lite_compatibility
    ));
    assert!(matches!(
        startup.providers.get(&lite),
        Some(BuiltinProviderProfile::Chatgpt(profile))
            if profile.responses_lite_compatibility
    ));
}

/// Chat Completions setup defaults to legacy token and streaming compatibility.
#[test]
fn chat_completions_add_defaults_to_legacy_max_tokens() {
    // The setup wizard is usually used for local OpenAI-compatible servers.
    // Those should get Tau's output cap through `max_tokens`, not OpenAI's
    // newer `max_completion_tokens` spelling.
    let compat = chat_completions_add_compat();

    assert!(!compat.max_completion_tokens);
    assert!(compat.stream_options);
    assert!(compat.openai_prompt_cache.is_none());
}

/// The Responses setup wizard must omit effort overrides so its generated
/// profiles use the complete public Responses default capability set.
#[test]
fn responses_add_omits_effort_override() {
    let models = parse_responses_model_list("gpt-5.4, gpt-5.4-mini").expect("model list");

    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|model| model.reasoning_effort.is_none()));
}

/// Persistent providers reject unknown fields instead of hiding schema
/// mistakes.
#[test]
fn provider_profiles_reject_unknown_fields() {
    // Provider profiles are user-authored persistent config. Unknown fields are
    // usually misspellings or stale schema, so accepting them hides mistakes.
    let error = serde_json::from_value::<BuiltinProviderProfile>(serde_json::json!({
        "kind": "chatgpt",
        "auth": {
            "access_token": "token",
            "extra": true,
        },
    }))
    .expect_err("profile auth should reject unknown fields");

    assert!(error.to_string().contains("unknown field"), "got: {error}");
}

fn test_chat_model(id: &str) -> ChatCompletionsModel {
    ChatCompletionsModel {
        id: ModelName::try_new(id.to_owned()).expect("valid model name"),
        display_name: None,
        context_window: tau_proto::TokenCount::new(128_000),
        max_input_tokens: None,
        max_output_tokens: None,
        compat: None,
        tags: Vec::new(),
        hosted_tool_capabilities: Vec::new(),
        supported_tool_types: vec![tau_proto::ToolType::Function],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        local_summary_compaction: None,
        cache_contract: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

/// Chat Completions profiles publish and route only explicitly configured
/// models.
#[test]
fn chat_completions_profiles_publish_and_route_only_configured_models() {
    // User-configured Chat Completions namespaces publish exactly their configured
    // models and reject unknown ids instead of falling back to another backend.
    let provider_name = ProviderName::new("local");
    let configured = test_chat_model("llama");
    let provider = ChatCompletionsProvider {
        cache_diagnostics: Default::default(),
        base_url: "http://127.0.0.1:8080/v1".to_owned(),
        api_key: String::new(),
        models: vec![configured.clone()],
        max_output_tokens: tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        tags: Vec::new(),
        compat: chat_completions_add_compat(),
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: Default::default(),
        missing_logins: Default::default(),
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::ChatCompletions(provider),
        )]),
    };
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();

    let models = models_for_profiles(&profiles);
    assert_eq!(model_ids(&models), vec!["local/llama"]);
    assert!(matches!(
        resolve_prompt_backend(
            &ModelId::new(provider_name.clone(), configured.id.clone()),
            &mut profiles,
            &mut refresh_rejections,
            &test_network_policy(),
                    None,
        ),
        Some(PromptBackend::ChatCompletions {
            provider,
            model_index,
        }) if provider.models[model_index].id == configured.id
    ));
    assert!(
        resolve_prompt_backend(
            &ModelId::new(provider_name, ModelName::new("missing")),
            &mut profiles,
            &mut refresh_rejections,
            &test_network_policy(),
            None,
        )
        .is_none()
    );
}

/// OpenRouter profiles publish and route only explicitly configured models.
#[test]
fn openrouter_profiles_publish_and_route_only_configured_models() {
    // OpenRouter profiles are wrapped into Chat Completions at dispatch time.
    // Keep coverage for both model publication and exact configured-model
    // routing so profile conversion does not accidentally widen access.
    let provider_name = ProviderName::new("openrouter");
    let configured = test_chat_model("anthropic/claude-test");
    let profile = OpenRouterProfile {
        cache_diagnostics: Default::default(),
        api_key: "key".to_owned(),
        models: vec![configured.clone()],
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: Default::default(),
        missing_logins: Default::default(),
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::OpenRouter(profile),
        )]),
    };
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();

    let models = models_for_profiles(&profiles);
    assert_eq!(model_ids(&models), vec!["openrouter/anthropic/claude-test"]);
    assert!(matches!(
        resolve_prompt_backend(
            &ModelId::new(provider_name.clone(), configured.id.clone()),
            &mut profiles,
            &mut refresh_rejections,
            &test_network_policy(),
                    None,
        ),
        Some(PromptBackend::ChatCompletions {
            provider,
            model_index,
        })
            if provider.base_url == "https://openrouter.ai/api/v1"
                && provider.models[model_index].id == configured.id
    ));
    assert!(
        resolve_prompt_backend(
            &ModelId::new(provider_name, ModelName::new("missing")),
            &mut profiles,
            &mut refresh_rejections,
            &test_network_policy(),
            None,
        )
        .is_none()
    );
}

/// A large generic route must retain one catalog allocation and select by
/// index.
///
/// This is a descriptive clone-work benchmark without a wall-clock threshold:
/// increasing the catalog exercises production resolution, while repeated
/// backend clones must share the same route allocation regardless of its size.
#[test]
fn large_catalog_backend_clones_share_one_indexed_route_snapshot() {
    const MODEL_COUNT: usize = 4_096;
    const CLONE_COUNT: usize = 1_024;
    let provider_name = ProviderName::new("large");
    let selected_index = MODEL_COUNT - 17;
    let models = (0..MODEL_COUNT)
        .map(|index| test_chat_model(&format!("model-{index}")))
        .collect::<Vec<_>>();
    let selected_id = models[selected_index].id.clone();
    let provider = ChatCompletionsProvider {
        base_url: "https://large.invalid/v1".to_owned(),
        api_key: "catalog-canary-bearer".to_owned(),
        models,
        ..ChatCompletionsProvider::default()
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: Default::default(),
        missing_logins: Default::default(),
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::ChatCompletions(provider),
        )]),
    };
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();
    let backend = resolve_prompt_backend_without_refresh(
        &ModelId::new(provider_name.clone(), selected_id.clone()),
        &mut profiles,
        &mut refresh_rejections,
    )
    .expect("selected model resolves");
    let PromptBackend::ChatCompletions {
        provider,
        model_index,
    } = &backend
    else {
        panic!("generic route resolves to Chat Completions");
    };
    assert_eq!(*model_index, selected_index);
    assert_eq!(provider.models[*model_index].id, selected_id);
    assert_eq!(provider.models.len(), MODEL_COUNT);
    assert!(
        matches!(
            profiles.providers.get(&provider_name),
            Some(BuiltinProviderProfile::ChatCompletions(moved)) if moved.models.is_empty()
        ),
        "resolution moves the catalog instead of cloning it"
    );

    for clone in std::iter::repeat_with(|| backend.clone()).take(CLONE_COUNT) {
        let PromptBackend::ChatCompletions {
            provider: cloned,
            model_index: cloned_index,
        } = clone
        else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(provider, &cloned));
        assert_eq!(cloned_index, selected_index);
    }
}

/// OpenRouter conversion must normalize a large catalog in place before the
/// shared indexed route takes ownership.
#[test]
fn large_openrouter_catalog_moves_into_indexed_route_snapshot() {
    const MODEL_COUNT: usize = 4_096;
    let provider_name = ProviderName::new("large-router");
    let selected_index = MODEL_COUNT / 2;
    let selected_id = ModelName::new(format!("route/model-{selected_index}"));
    let profile = OpenRouterProfile {
        cache_diagnostics: Default::default(),
        api_key: "router-content-canary-bearer".to_owned(),
        models: (0..MODEL_COUNT)
            .map(|index| test_chat_model(&format!("route/model-{index}")))
            .collect(),
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: Default::default(),
        missing_logins: Default::default(),
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::OpenRouter(profile),
        )]),
    };
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();
    let backend = resolve_prompt_backend_without_refresh(
        &ModelId::new(provider_name.clone(), selected_id.clone()),
        &mut profiles,
        &mut refresh_rejections,
    )
    .expect("selected OpenRouter model resolves");
    let PromptBackend::ChatCompletions {
        provider,
        model_index,
    } = backend
    else {
        panic!("OpenRouter resolves to Chat Completions");
    };
    assert_eq!(model_index, selected_index);
    assert_eq!(provider.models[model_index].id, selected_id);
    assert_eq!(provider.models.len(), MODEL_COUNT);
    assert!(
        matches!(
            profiles.providers.get(&provider_name),
            Some(BuiltinProviderProfile::OpenRouter(moved)) if moved.models.is_empty()
                && moved.api_key.is_empty()
        ),
        "OpenRouter resolution moves the catalog and bearer instead of cloning them"
    );
}

/// Persistent failures must retain unbounded attempt accounting while generated
/// retry delays cap at the approved thirty-minute ceiling.
#[test]
fn generated_retry_delay_caps_without_exhausting_attempts() {
    let mut state = PromptRetryState::default();
    for _ in 0..10_000 {
        let delay = state.next_delay(RetryClass::Unknown, "ap-persistent");
        assert!(delay <= Duration::from_secs(30 * 60));
    }
    assert_eq!(state.attempts, 10_000);
}

/// Standalone compaction must stop after its named five-attempt policy while
/// ordinary inference retains deliberately unbounded retry authority.
#[test]
fn standalone_retry_budget_exhausts_without_bounding_inference() {
    let compaction =
        PromptRetryPolicy::for_operation(tau_proto::PromptOperation::StandaloneCompaction);
    assert_eq!(
        compaction,
        PromptRetryPolicy::FiveAttemptStandaloneCompaction
    );
    assert_eq!(compaction.after_failure(3), PromptRetryDisposition::Retry);
    assert_eq!(
        compaction.after_failure(4),
        PromptRetryDisposition::Terminal(STANDALONE_COMPACTION_ATTEMPT_LIMIT)
    );

    let inference = PromptRetryPolicy::for_operation(tau_proto::PromptOperation::Inference);
    assert_eq!(inference, PromptRetryPolicy::UnboundedInference);
    assert_eq!(
        inference.after_failure(u64::MAX),
        PromptRetryDisposition::Retry
    );
}

/// Ensures prompts sharing one reset lower bound receive positive stable
/// prompt-local jitter instead of stampeding at one identical instant.
#[test]
fn shared_cooldown_jitter_is_positive_stable_and_prompt_local() {
    let first = cooldown_jitter("ap-first", 7);
    let first_again = cooldown_jitter("ap-first", 7);
    let second = cooldown_jitter("ap-second", 7);
    assert!(first > Duration::ZERO);
    assert!(first <= RESET_BOUNDARY_JITTER_MAX);
    assert_eq!(first, first_again);
    assert_ne!(first, second);
}

/// Ensures a targeted cancellation remains observable until a late worker
/// retry outcome is rejected rather than resurrecting delayed work.
#[test]
fn cancellation_state_reports_pending_target_without_consuming_it() {
    let cancellation = CancellationState::default();
    let prompt_id = tau_proto::AgentPromptId::parse("ap-late-retry")
        .expect("known-safe AgentPromptId must be valid");
    cancellation.cancel(prompt_id.clone());
    assert!(cancellation.is_canceled(&prompt_id));
    assert!(cancellation.is_canceled(&prompt_id));
    assert!(cancellation.take_canceled(&prompt_id));
    assert!(!cancellation.is_canceled(&prompt_id));
}

/// Targeted cancellation must wake only the matching parked backend turn rather
/// than relying on its periodic stream receive timeout.
#[test]
fn cancellation_waker_fires_for_matching_prompt_only() {
    let cancellation = Arc::new(CancellationState::default());
    let target_apid = tau_proto::AgentPromptId::parse("ap-target")
        .expect("known-safe AgentPromptId must be valid");
    let other_apid = tau_proto::AgentPromptId::parse("ap-other")
        .expect("known-safe AgentPromptId must be valid");
    let matching = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let other = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));

    let cancel_generation = cancellation.retry_generation();
    let _matching_guard = cancellation.register_abort_waker(&target_apid, cancel_generation, {
        let matching = Arc::clone(&matching);
        Arc::new(move || {
            matching.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });
    let _other_guard = cancellation.register_abort_waker(&other_apid, cancel_generation, {
        let other = Arc::clone(&other);
        Arc::new(move || {
            other.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });

    cancellation.cancel(target_apid);

    assert_eq!(matching.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(other.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Ensures broadcast cancellation wakes every registered active backend while
/// advancing the generation observed by retry and transport abort checks.
#[test]
fn cancellation_global_cancel_wakes_all_registered_abort_wakers() {
    let cancellation = Arc::new(CancellationState::default());
    let first_apid = tau_proto::AgentPromptId::parse("ap-first")
        .expect("known-safe AgentPromptId must be valid");
    let second_apid = tau_proto::AgentPromptId::parse("ap-second")
        .expect("known-safe AgentPromptId must be valid");
    let first = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let second = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let initial_generation = cancellation.retry_generation();

    let _first_guard = cancellation.register_abort_waker(&first_apid, initial_generation, {
        let first = Arc::clone(&first);
        Arc::new(move || {
            first.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });
    let _second_guard = cancellation.register_abort_waker(&second_apid, initial_generation, {
        let second = Arc::clone(&second);
        Arc::new(move || {
            second.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });

    cancellation.cancel_all();

    assert_ne!(cancellation.retry_generation(), initial_generation);
    assert_eq!(first.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(second.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Ensures a backend registering after broadcast cancellation observes its
/// stale generation immediately instead of losing the cancellation wakeup.
#[test]
fn cancellation_global_cancel_wakes_late_old_generation_registration() {
    let cancellation = Arc::new(CancellationState::default());
    let prompt_id = tau_proto::AgentPromptId::parse("ap-late-registration")
        .expect("known-safe AgentPromptId must be valid");
    let stale_generation = cancellation.retry_generation();
    let calls = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));

    cancellation.cancel_all();
    let _guard = cancellation.register_abort_waker(&prompt_id, stale_generation, {
        let calls = Arc::clone(&calls);
        Arc::new(move || {
            calls.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Provider shutdown must wake every active WebSocket turn so workers return
/// their normal canceled terminal rather than waiting on idle sockets.
#[test]
fn cancellation_shutdown_wakes_all_registered_abort_wakers() {
    let cancellation = Arc::new(CancellationState::default());
    let first_apid = tau_proto::AgentPromptId::parse("ap-first")
        .expect("known-safe AgentPromptId must be valid");
    let second_apid = tau_proto::AgentPromptId::parse("ap-second")
        .expect("known-safe AgentPromptId must be valid");
    let first = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let second = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));

    let cancel_generation = cancellation.retry_generation();
    let _first_guard = cancellation.register_abort_waker(&first_apid, cancel_generation, {
        let first = Arc::clone(&first);
        Arc::new(move || {
            first.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });
    let _second_guard = cancellation.register_abort_waker(&second_apid, cancel_generation, {
        let second = Arc::clone(&second);
        Arc::new(move || {
            second.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });

    cancellation.shutdown();

    assert_eq!(first.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(second.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Dropping a completed turn's abort-waker guard must prevent later
/// cancellation from delivering stale wake hints into a reused socket stream.
#[test]
fn cancellation_waker_guard_unregisters_on_drop() {
    let cancellation = Arc::new(CancellationState::default());
    let apid =
        tau_proto::AgentPromptId::parse("ap-drop").expect("known-safe AgentPromptId must be valid");
    let calls = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));

    let guard = cancellation.register_abort_waker(&apid, cancellation.retry_generation(), {
        let calls = Arc::clone(&calls);
        Arc::new(move || {
            calls.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        })
    });
    drop(guard);

    cancellation.cancel(apid);

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn minimal_prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        system_prompt: String::new(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        hosted_tools: Vec::new(),
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

/// Returns a valid ChatGPT OAuth value for prompt-admission state tests.
fn prompt_async_test_auth() -> OpenAiAuth {
    OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: now_ms().saturating_add(3_600_000),
        account_id: Some("account-a".to_owned()),
    }
}

/// Builds an inert runtime owner for focused credential-admission transitions.
fn observation_test_runtime()
-> ProviderRuntime<impl FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles> {
    let (worker_tx, worker_rx) = mpsc::channel();
    ProviderRuntime {
        load_prompt_profiles: |_: Option<&ProviderName>| BuiltinProviderProfiles::default(),
        startup_responses_modes: BTreeMap::new(),
        prompt_concurrency_limit: 0,
        prompt_executor: production_prompt_executor(),
        prewarm_executor: production_prewarm_executor(),
        worker_tx,
        worker_rx,
        worker_waker: None,
        retry_scheduler: None,
        credential_admission: PromptCredentialAdmissionState::default(),
        retry_clock: Arc::new(SystemRetryClock),
        shared_cooldowns: BTreeMap::new(),
        shared_cooldown_generation: 0,
        codex_runtime: Arc::new(CodexRuntime::new(Arc::new(test_network_policy()))),
        prewarm_supervisor: PrewarmSupervisor::default(),
        provider_profile_identities: BTreeMap::new(),
        prewarm_profile_identities: BTreeMap::new(),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        active_prompts: 0,
        input_closed: false,
        cancel_generation: 0,
        quota: QuotaCoordinator::default(),
        oauth_refresh_rejections: OAuthRefreshRejectionCache::default(),
        unavailable_compact_identities: HashSet::new(),
        compact_profile_identities: HashMap::new(),
        extension_data_client: None,
        declared_credential_observations: None,
        declared_models: None,
        diagnostics: ProviderDiagnosticsState::default(),
    }
}

/// Ensures admission completion never re-enters synchronous OAuth refresh:
/// a still-valid token that is already inside the proactive refresh window
/// remains usable after the asynchronous refresh state machine fails.
#[test]
fn admitted_backend_uses_still_valid_refresh_due_oauth_without_io() {
    let auth = OpenAiAuth {
        access_token: oauth_test_jwt("account-a"),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: now_ms().saturating_add(60_000),
        account_id: Some("account-a".to_owned()),
    };
    assert!(oauth_token_should_refresh(
        &auth.access_token,
        auth.expires_at_ms
    ));
    let mut profiles = profiles_with_chatgpt_auth(auth);
    let model = ModelId::new(
        ProviderName::new("chatgpt"),
        ModelName::new("gpt-5.3-codex"),
    );
    let backend = resolve_prompt_backend_without_refresh(
        &model,
        &mut profiles,
        &mut OAuthRefreshRejectionCache::default(),
    );
    assert!(matches!(backend, Some(PromptBackend::Responses(_))));
}

/// Credential deadlines share one deterministic min-heap: cancellation
/// invalidates a late timeout, equal deadlines retain arm order, and clearing
/// the session prevents any old request from firing.
#[test]
fn prompt_credential_deadline_queue_orders_and_invalidates_without_sleeping() {
    let origin = Instant::now();
    let mut queue = PromptCredentialDeadlineQueue::default();
    queue.apply(PromptCredentialDeadlineCommand::Schedule {
        request_id: "late".to_owned(),
        due: origin + Duration::from_secs(2),
    });
    queue.apply(PromptCredentialDeadlineCommand::Schedule {
        request_id: "first".to_owned(),
        due: origin + Duration::from_secs(1),
    });
    queue.apply(PromptCredentialDeadlineCommand::Schedule {
        request_id: "second".to_owned(),
        due: origin + Duration::from_secs(1),
    });
    queue.apply(PromptCredentialDeadlineCommand::Cancel("late".to_owned()));

    assert!(queue.pop_due(origin).is_empty());
    assert_eq!(
        queue.pop_due(origin + Duration::from_secs(1)),
        ["first", "second"]
    );
    queue.apply(PromptCredentialDeadlineCommand::Schedule {
        request_id: "shutdown".to_owned(),
        due: origin + Duration::from_secs(3),
    });
    queue.apply(PromptCredentialDeadlineCommand::CancelAll);
    assert_eq!(queue.next_due(), None);
    assert!(queue.pop_due(origin + Duration::from_secs(10)).is_empty());
}

/// OAuth refresh coalescing is scoped to the complete provider/path/generation/
/// mode key: exact keys join, while every authority-bearing component splits
/// the operation.
#[test]
fn prompt_oauth_refresh_key_coalesces_exactly_and_not_across_generations() {
    let key = PromptOAuthRefreshKey {
        provider: ProviderName::new("chatgpt"),
        path: tau_proto::ExtensionDataPath::new("providers/identity/oauth.json"),
        generation: "generation-a".to_owned(),
        lite_compatibility: false,
    };
    let mut refreshes = HashMap::from([(
        key.clone(),
        PromptOAuthRefresh {
            current: prompt_async_test_auth(),
            forced: false,
            transport_finished: false,
            secret_in_flight: false,
        },
    )]);
    assert!(refreshes.contains_key(&key));

    for distinct in [
        PromptOAuthRefreshKey {
            provider: ProviderName::new("other"),
            ..key.clone()
        },
        PromptOAuthRefreshKey {
            path: tau_proto::ExtensionDataPath::new("providers/other/oauth.json"),
            ..key.clone()
        },
        PromptOAuthRefreshKey {
            generation: "generation-b".to_owned(),
            ..key.clone()
        },
        PromptOAuthRefreshKey {
            lite_compatibility: true,
            ..key.clone()
        },
    ] {
        assert!(!refreshes.contains_key(&distinct));
        refreshes.insert(
            distinct,
            PromptOAuthRefresh {
                current: prompt_async_test_auth(),
                forced: false,
                transport_finished: false,
                secret_in_flight: false,
            },
        );
    }
    assert_eq!(refreshes.len(), 5);
}

/// The production OAuth failure owner must close a non-default timed class
/// without rendering its key, credential, provider, path, or error inputs.
#[test]
fn production_prompt_oauth_failure_closes_private_observation() {
    let mut runtime = observation_test_runtime();
    runtime.diagnostics.receipt.suppress_oauth_worker = true;
    let provider = ProviderName::new(CHATGPT_PROVIDER_NAME);
    let auth = OpenAiAuth {
        expires_at_ms: now_ms().saturating_add(60_000),
        refresh_token: "refresh-token-canary".to_owned(),
        ..prompt_async_test_auth()
    };
    let mut prompt = minimal_prompt();
    prompt.model = ModelId::new(provider.clone(), ModelName::new("gpt-5.3-codex"));
    let observation = ReceiptObservation::new(tau_client::LocalInputObservation {
        frame_bytes: tau_proto::ProtocolMessageBytes::new(1).expect("nonzero frame"),
        decode_elapsed: Duration::ZERO,
        decoded_at: Instant::now(),
    });
    let generation = blake3::hash(b"oauth-generation-canary");
    runtime
        .credential_admission
        .admissions
        .push_back(PendingPromptAdmission {
            kind: PendingPromptAdmissionKind::Initial {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                prompt,
            },
            profiles: profiles_with_chatgpt_auth(auth.clone()),
            request_id: Some("oauth-request-canary".to_owned()),
            observations: Some(BTreeMap::from([(
                provider,
                CredentialObservation::Contents(generation),
            )])),
            oauth_refresh: None,
            oauth_forced: false,
            receipt_observation: Some(observation),
        });
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.receipt=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        runtime.stage_prompt_oauth_refresh("oauth-request-canary");
        let key = runtime
            .credential_admission
            .admissions
            .front()
            .and_then(|admission| admission.oauth_refresh.clone())
            .expect("real OAuth start installed a flight");
        std::thread::sleep(Duration::from_millis(1));
        runtime.fail_prompt_oauth_refresh(&key, None);
        let observation = runtime
            .credential_admission
            .admissions
            .front_mut()
            .expect("OAuth admission")
            .receipt_observation
            .take()
            .expect("receipt observation");
        observation.finished_before_worker(ReceiptOutcome::Canceled);

        runtime.credential_admission.admissions.clear();
        let mut prompt = minimal_prompt();
        prompt.model = ModelId::new(
            ProviderName::new(CHATGPT_PROVIDER_NAME),
            ModelName::new("gpt-5.3-codex"),
        );
        runtime
            .credential_admission
            .admissions
            .push_back(PendingPromptAdmission {
                kind: PendingPromptAdmissionKind::Initial {
                    agent_prompt_id: prompt.agent_prompt_id.clone(),
                    prompt,
                },
                profiles: profiles_with_chatgpt_auth(auth.clone()),
                request_id: Some("oauth-refresh-request-canary".to_owned()),
                observations: Some(BTreeMap::from([(
                    ProviderName::new(CHATGPT_PROVIDER_NAME),
                    CredentialObservation::Contents(generation),
                )])),
                oauth_refresh: None,
                oauth_forced: false,
                receipt_observation: Some(ReceiptObservation::new(
                    tau_client::LocalInputObservation {
                        frame_bytes: tau_proto::ProtocolMessageBytes::new(1)
                            .expect("nonzero frame"),
                        decode_elapsed: Duration::ZERO,
                        decoded_at: Instant::now(),
                    },
                )),
            });
        runtime.stage_prompt_oauth_refresh("oauth-refresh-request-canary");
        let key = runtime
            .credential_admission
            .admissions
            .front()
            .and_then(|admission| admission.oauth_refresh.clone())
            .expect("second real OAuth start installed a flight");
        {
            let refresh = runtime
                .credential_admission
                .oauth_refreshes
                .get_mut(&key)
                .expect("shared OAuth flight");
            refresh.transport_finished = true;
            refresh.secret_in_flight = true;
        }
        let late_profiles = runtime
            .credential_admission
            .admissions
            .front()
            .expect("shared OAuth admission")
            .profiles
            .clone();
        let mut late_prompt = minimal_prompt();
        late_prompt.model = ModelId::new(
            ProviderName::new(CHATGPT_PROVIDER_NAME),
            ModelName::new("gpt-5.3-codex"),
        );
        runtime
            .credential_admission
            .admissions
            .push_back(PendingPromptAdmission {
                kind: PendingPromptAdmissionKind::Initial {
                    agent_prompt_id: late_prompt.agent_prompt_id.clone(),
                    prompt: late_prompt,
                },
                profiles: late_profiles,
                request_id: Some("oauth-late-join-canary".to_owned()),
                observations: Some(BTreeMap::from([(
                    ProviderName::new(CHATGPT_PROVIDER_NAME),
                    CredentialObservation::Contents(generation),
                )])),
                oauth_refresh: None,
                oauth_forced: false,
                receipt_observation: Some(ReceiptObservation::new(
                    tau_client::LocalInputObservation {
                        frame_bytes: tau_proto::ProtocolMessageBytes::new(1)
                            .expect("nonzero frame"),
                        decode_elapsed: Duration::ZERO,
                        decoded_at: Instant::now(),
                    },
                )),
            });
        runtime.stage_prompt_oauth_refresh("oauth-late-join-canary");
        std::thread::sleep(Duration::from_millis(1));
        let late_observation = runtime
            .credential_admission
            .admissions
            .back_mut()
            .expect("late OAuth admission")
            .receipt_observation
            .take()
            .expect("late receipt observation");
        late_observation.finished_before_worker(ReceiptOutcome::Canceled);
        runtime.complete_prompt_oauth_refresh(&key, auth.clone());
        let observation = runtime
            .credential_admission
            .admissions
            .front_mut()
            .expect("refreshed OAuth admission")
            .receipt_observation
            .take()
            .expect("refreshed receipt observation");
        observation.finished_before_worker(ReceiptOutcome::Canceled);
    });
    let trace = String::from_utf8(trace.bytes()).expect("UTF-8 trace");

    assert!(trace.contains("oauth_class=\"failed\""), "{trace}");
    assert!(trace.contains("oauth_class=\"refreshed\""), "{trace}");
    assert!(
        trace.lines().any(|line| {
            line.contains("oauth_class=\"failed\"")
                && !line.contains("oauth_us=0")
                && line.contains("secret_rpc_count=0")
        }),
        "started OAuth failure was not timed: {trace}"
    );
    assert!(
        trace.lines().any(|line| {
            line.contains("oauth_class=\"refreshed\"") && !line.contains("oauth_us=0")
        }),
        "started OAuth refresh was not timed: {trace}"
    );
    assert!(
        trace.lines().any(|line| {
            line.contains("secret_rpc_count=1")
                && !line.contains("secret_wait_us=0")
                && line.contains("oauth_class=\"failed\"")
        }),
        "late CAS/reload join was not timed: {trace}"
    );
    for canary in [
        "oauth-generation-canary",
        "oauth-request-canary",
        "oauth-refresh-request-canary",
        "oauth-late-join-canary",
        "refresh-token-canary",
        "account-a",
    ] {
        assert!(!trace.contains(canary), "{canary} leaked: {trace}");
    }
}

/// A second 401 waiter for an exact generation joins the forced flight even
/// after the first waiter consumed that generation's recovery authority.
#[test]
fn prompt_oauth_exhausted_waiter_joins_matching_forced_flight() {
    let provider = ProviderName::new("chatgpt");
    let identity = BackendProfileIdentity::from_test_value(19);
    let mut rejections = OAuthRefreshRejectionCache::default();
    rejections.record_unauthorized(provider.clone(), identity);
    assert!(rejections.take_unauthorized(&provider, identity));
    assert!(rejections.unauthorized_exhausted(&provider, identity));

    let flight = PromptOAuthRefresh {
        current: prompt_async_test_auth(),
        forced: true,
        transport_finished: false,
        secret_in_flight: false,
    };
    let joined_forced = flight.forced || rejections.take_unauthorized(&provider, identity);
    assert!(
        joined_forced,
        "in-flight forced authority applies to every exact-generation waiter"
    );
}

/// A CAS loser observes the winner through the same authoritative reload used
/// after CAS success; unrelated payloads fail closed instead.
#[test]
fn prompt_oauth_cas_loser_and_winner_both_require_authoritative_reload() {
    assert!(prompt_oauth_cas_requires_reload(
        &tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
        }
    ));
    assert!(prompt_oauth_cas_requires_reload(
        &tau_proto::ExtensionDataResultPayload::Error {
            kind: tau_proto::ExtensionDataErrorKind::GenerationMismatch,
            message: "lost CAS".to_owned(),
        }
    ));
    assert!(!prompt_oauth_cas_requires_reload(
        &tau_proto::ExtensionDataResultPayload::Error {
            kind: tau_proto::ExtensionDataErrorKind::Io,
            message: "failed".to_owned(),
        }
    ));
}

/// Canceling one coalesced OAuth waiter must retain the shared operation for
/// its sibling; only removal of the final exact-key waiter invalidates it.
#[test]
fn prompt_oauth_shared_refresh_survives_one_waiter_cancellation() {
    let key = PromptOAuthRefreshKey {
        provider: ProviderName::new("chatgpt"),
        path: tau_proto::ExtensionDataPath::new("providers/identity/oauth.json"),
        generation: "generation".to_owned(),
        lite_compatibility: false,
    };
    let admission = |agent_prompt_id: &str| {
        let mut prompt = minimal_prompt();
        prompt.agent_prompt_id = agent_prompt_id.parse().expect("valid prompt id");
        PendingPromptAdmission {
            kind: PendingPromptAdmissionKind::Initial {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                prompt,
            },
            profiles: BuiltinProviderProfiles::default(),
            request_id: None,
            observations: Some(BTreeMap::new()),
            oauth_refresh: Some(key.clone()),
            oauth_forced: false,
            receipt_observation: None,
        }
    };
    let mut admissions = VecDeque::from([admission("waiter-a"), admission("waiter-b")]);

    admissions.pop_front();
    assert!(prompt_oauth_has_waiter(&admissions, &key));
    admissions.pop_front();
    assert!(!prompt_oauth_has_waiter(&admissions, &key));
}

/// Read and CAS deadlines invalidate their exact correlations once. A late
/// harness reply or duplicate timer notification cannot re-enter either state.
#[test]
fn prompt_credential_read_and_cas_timeouts_ignore_late_reentry() {
    let mut prompt = minimal_prompt();
    prompt.agent_prompt_id = "timed-read".parse().expect("valid prompt id");
    let mut admissions = VecDeque::from([PendingPromptAdmission {
        kind: PendingPromptAdmissionKind::Initial {
            agent_prompt_id: prompt.agent_prompt_id.clone(),
            prompt,
        },
        profiles: BuiltinProviderProfiles::default(),
        request_id: Some("read-rpc".to_owned()),
        observations: None,
        oauth_refresh: None,
        oauth_forced: false,
        receipt_observation: None,
    }]);
    let mut oauth_rpcs = HashMap::new();

    assert_eq!(
        apply_prompt_credential_timeout("read-rpc", &mut oauth_rpcs, &mut admissions),
        None
    );
    assert!(admissions[0].observations.is_some());
    assert_eq!(admissions[0].request_id, None);
    assert_eq!(
        apply_prompt_credential_timeout("read-rpc", &mut oauth_rpcs, &mut admissions),
        None,
        "duplicate timeout and late reply correlation stay invalid"
    );

    let key = PromptOAuthRefreshKey {
        provider: ProviderName::new("chatgpt"),
        path: tau_proto::ExtensionDataPath::new("providers/identity/oauth.json"),
        generation: "generation".to_owned(),
        lite_compatibility: false,
    };
    oauth_rpcs.insert(
        "cas-rpc".to_owned(),
        PromptOAuthRpc::CompareAndSwap { key: key.clone() },
    );
    assert_eq!(
        apply_prompt_credential_timeout("cas-rpc", &mut oauth_rpcs, &mut admissions),
        Some(key)
    );
    assert_eq!(
        apply_prompt_credential_timeout("cas-rpc", &mut oauth_rpcs, &mut admissions),
        None,
        "late CAS result cannot resurrect the continuation"
    );
}

/// Startup quota selection consumes one provider per reactive turn. This leaves
/// a prompt-drain boundary between providers instead of letting quota startup
/// monopolize the loop.
#[test]
fn initial_quota_selection_yields_between_providers_for_prompt_input() {
    let mut profiles = profiles_with_chatgpt_auth(prompt_async_test_auth());
    let chatgpt = profiles
        .providers
        .get(&ProviderName::new("chatgpt"))
        .expect("ChatGPT profile")
        .clone();
    profiles
        .providers
        .insert(ProviderName::new("chatgpt-second"), chatgpt);
    let mut startup = Some(profiles);

    let first = take_next_initial_quota_profile(&mut startup).expect("first quota provider");
    assert_eq!(first.providers.len(), 1);
    assert!(
        startup.is_some(),
        "another provider remains, but the caller regains the reactive loop"
    );

    let second = take_next_initial_quota_profile(&mut startup).expect("second quota provider");
    assert_eq!(second.providers.len(), 1);
    assert!(startup.is_none());
}

/// Due and manually released retries each invoke the mutable profile loader at
/// their own transition, so neither inherits the prior attempt's credential
/// generation.
#[test]
fn due_and_manual_retry_transitions_each_load_fresh_credentials() {
    let calls = Cell::new(0_u64);
    let mut loader = |selected: Option<&ProviderName>| {
        assert_eq!(selected, Some(&ProviderName::new("chatgpt")));
        let generation = calls.get() + 1;
        calls.set(generation);
        profiles_with_chatgpt_auth(OpenAiAuth {
            access_token: format!("generation-{generation}"),
            ..prompt_async_test_auth()
        })
    };
    let provider = ProviderName::new("chatgpt");
    let modes = BTreeMap::from([(provider.clone(), CodexMode::Standard)]);

    let due = load_fresh_retry_profiles(&mut loader, &modes, &provider);
    let manual = load_fresh_retry_profiles(&mut loader, &modes, &provider);
    let access_token = |profiles: &BuiltinProviderProfiles| {
        let Some(BuiltinProviderProfile::Chatgpt(profile)) = profiles.providers.get(&provider)
        else {
            panic!("ChatGPT profile")
        };
        profile.auth.access_token.clone()
    };
    assert_eq!(access_token(&due), "generation-1");
    assert_eq!(access_token(&manual), "generation-2");
    assert_eq!(calls.get(), 2);
}

/// Provider materialization must consume the handler-owned prompt allocation,
/// retain every large payload allocation, and clear only the transport reuse
/// reference.
#[test]
fn provider_materialization_moves_owned_prompt_without_payload_clones() {
    let mut prompt = minimal_prompt();
    prompt.system_prompt = "owned-system-prompt".repeat(256);
    prompt.context.blocks = vec![tau_proto::ContextBlock::UserInput(
        tau_proto::UserInputBlock {
            items: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::User,
                content: vec![tau_proto::ContentPart::Text {
                    text: "owned-context".repeat(256),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        },
    )];
    prompt.tools = vec![tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("owned_tool"),
        model_visible_name: None,
        description: Some("owned-description".repeat(256)),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    }];
    prompt.tools_ref = Some(tau_proto::PromptToolsRef {
        base_agent_prompt_id: tau_proto::AgentPromptId::parse("ap-base")
            .expect("known-safe prompt id"),
    });

    let system_prompt_ptr = prompt.system_prompt.as_ptr();
    let context_ptr = prompt.context.blocks.as_ptr();
    let tools_ptr = prompt.tools.as_ptr();
    let materialized = materialize_prompt(prompt);

    let retained_allocation_count = [
        materialized.system_prompt.as_ptr() == system_prompt_ptr,
        materialized.context.blocks.as_ptr().cast::<u8>() == context_ptr.cast::<u8>(),
        materialized.tools.as_ptr().cast::<u8>() == tools_ptr.cast::<u8>(),
    ]
    .into_iter()
    .filter(|retained| *retained)
    .count();
    assert_eq!(
        retained_allocation_count, 3,
        "all three independently allocated prompt surfaces must move without cloning"
    );
    assert_eq!(materialized.tools_ref, None);
}

/// Every report variant converted to typed worker delivery must remain
/// byte-for-byte identical to the ordinary peer writer and preserve FIFO order.
#[test]
fn typed_worker_reports_match_wire_roundtrip_for_every_converted_variant() {
    #[derive(Clone, Default)]
    struct RecordingWaker(Arc<path_std_sync_atomic::AtomicUsize>);

    impl WorkerReportWaker for RecordingWaker {
        fn wake_provider_loop(&self) {
            self.0.fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
        }
    }

    let prompt = minimal_prompt();
    let messages = vec![
        HarnessInputMessage::emit_transient(Event::ProviderPromptSubmittedReported(
            ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        )),
        HarnessInputMessage::emit_transient(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                agent_id: prompt.agent_id.clone(),
                deltas: Vec::new(),
                compaction: None,
                status: Some(ProviderResponseStatusUpdate {
                    text: "typed update".to_owned(),
                    clear_response: false,
                    retry: None,
                    native_tool: None,
                }),
                response_stats: Some(tau_proto::ProviderResponseStats {
                    current: tau_proto::ProviderResponseStatsSample {
                        response_bytes_received: 4096,
                        elapsed_micros: 2000,
                    },
                    previous: tau_proto::ProviderResponseStatsSample {
                        response_bytes_received: 1024,
                        elapsed_micros: 1000,
                    },
                    first_semantic_output_elapsed_micros: Some(750),
                }),
                originator: prompt.originator.clone(),
            },
        )),
        HarnessInputMessage::emit_transient(Event::ProviderCacheMissDiagnosticReported(
            tau_proto::ProviderCacheMissDiagnostic {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                model: prompt.model.clone(),
                originator: prompt.originator.clone(),
                tool_choice: prompt.tool_choice,
                ws_pool_delta: None,
                input_tokens: 200,
                cached_tokens: 10,
                previous_input_tokens: 180,
                cacheable_input_tokens: 160,
                corrected_cache_efficiency: 0.0625,
            },
        )),
        HarnessInputMessage::emit_transient(Event::ProviderResponseFinishedReported(
            simple_finished(
                prompt.agent_prompt_id.clone(),
                prompt.agent_id.clone(),
                prompt.originator.clone(),
                "typed finish",
            ),
        )),
        HarnessInputMessage::emit_transient(Event::ProviderRetryPromptResultReported(
            tau_proto::ProviderRetryPromptResult {
                request_id: tau_proto::RetryPromptRequestId::parse("retry-result")
                    .expect("known-safe retry request id"),
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                status: tau_proto::RetryPromptStatus::Accepted,
            },
        )),
    ];

    let (tx, rx) = mpsc::channel();
    let waker = RecordingWaker::default();
    let mut sink = WorkerReportSink {
        tx,
        waker: waker.clone(),
        worker_output_depth: None,
        cancel_generation: 7,
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        cooldown_probe: None,
    };
    for message in messages.iter().cloned() {
        sink.send_report(message).expect("admit typed report");
    }
    let direct = rx
        .try_iter()
        .map(|worker| {
            let WorkerMessage::Output {
                output,
                cancel_generation,
                agent_prompt_id,
                cooldown_probe,
                ..
            } = worker
            else {
                panic!("report sink emitted non-output worker message");
            };
            assert_eq!(cancel_generation, 7);
            assert_eq!(agent_prompt_id, prompt.agent_prompt_id);
            assert!(cooldown_probe.is_none());
            output.message().clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        waker.0.load(path_std_sync_atomic::Ordering::SeqCst),
        messages.len()
    );
    assert_eq!(
        direct, messages,
        "typed admission must preserve exact FIFO values"
    );

    let mut expected_wire = Vec::new();
    let mut trait_wire = Vec::new();
    {
        let mut expected = tau_proto::PeerOutputWriter::new(&mut expected_wire);
        let mut through_trait = tau_proto::PeerOutputWriter::new(&mut trait_wire);
        for message in &messages {
            expected
                .write_message(message)
                .expect("encode expected report");
            expected.flush().expect("flush expected report");
            through_trait
                .send_report(message.clone())
                .expect("encode report through compatibility sink");
        }
    }
    assert_eq!(
        trait_wire, expected_wire,
        "remote peer transport bytes must not change"
    );
    assert_eq!(decode_frames(&trait_wire), direct);
}

/// Describes report-count and payload-size scaling for the owned typed handoff
/// against the removed encode-buffer-decode worker roundtrip.
#[test]
#[ignore = "descriptive performance benchmark"]
fn benchmark_owned_provider_report_handoff_scaling() {
    use std::hint::black_box;
    use std::time::Instant;

    eprintln!(
        "reports,payload_bytes,typed_elapsed_ns,wire_roundtrip_elapsed_ns,wire_intermediate_bytes,checksum"
    );
    for report_count in [1_usize, 64, 4096] {
        for payload_bytes in [0_usize, 4 * 1024, 256 * 1024] {
            let message = HarnessInputMessage::emit_transient(
                Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
                    agent_prompt_id: tau_proto::AgentPromptId::parse("ap-benchmark")
                        .expect("known-safe prompt id"),
                    agent_id: tau_proto::AgentId::parse("agent-benchmark")
                        .expect("known-safe agent id"),
                    deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                        output_index: 0,
                        text: "x".repeat(payload_bytes),
                        phase: None,
                    }],
                    compaction: None,
                    status: None,
                    response_stats: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            );

            let typed_started = Instant::now();
            let mut typed_checksum = 0_u64;
            for _ in 0..report_count {
                let report =
                    prepare_worker_report(message.clone()).expect("admit benchmark typed report");
                typed_checksum = typed_checksum.wrapping_add(report.encoded_bytes());
                black_box(report);
            }
            let typed_elapsed = typed_started.elapsed();

            let wire_started = Instant::now();
            let mut wire_intermediate_bytes = 0_u64;
            let mut wire_checksum = 0_u64;
            for _ in 0..report_count {
                let report = message.clone();
                let mut bytes = Vec::new();
                tau_proto::PeerOutputWriter::new(&mut bytes)
                    .write_message(&report)
                    .expect("encode benchmark wire report");
                wire_intermediate_bytes =
                    wire_intermediate_bytes.saturating_add(bytes.len() as u64);
                let decoded = decode_frames(&bytes);
                wire_checksum = wire_checksum.wrapping_add(
                    tau_client::encoded_outbound_frame_bytes(
                        decoded.first().expect("one decoded benchmark report"),
                    )
                    .expect("measure decoded benchmark report"),
                );
                black_box(decoded);
            }
            let wire_elapsed = wire_started.elapsed();
            assert_eq!(typed_checksum, wire_checksum);
            eprintln!(
                "{report_count},{payload_bytes},{},{},{wire_intermediate_bytes},{typed_checksum}",
                typed_elapsed.as_nanos(),
                wire_elapsed.as_nanos(),
            );
        }
    }
}

/// Native compact usage must retain cache-read and cache-write observations
/// when the adapter turns a finished compact outcome into its reported event.
#[test]
fn compact_finished_response_preserves_native_cache_usage() {
    let prompt = minimal_prompt();
    let usage = tau_proto::ProviderTokenUsage {
        model: Some(prompt.model.clone()),
        prompt_sent_tokens: 120,
        prompt_cached_tokens: 80,
        prompt_cache_read_ceiling_tokens: Some(100),
        cache: Some(Box::new(tau_proto::ProviderCacheUsage {
            read_tokens: Some(80),
            write_tokens: Some(20),
            ..Default::default()
        })),
        response_received_tokens: 7,
        stats: Default::default(),
    };
    let finished = compact_finished_response(
        &prompt.agent_prompt_id,
        &prompt,
        tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::Responses,
            base_url: "https://example.invalid".to_owned(),
            transport: tau_proto::ProviderBackendTransport::Websocket,
            stale_chain_fallback: false,
        },
        Vec::new(),
        Some(usage.clone()),
        tau_proto::ProviderAttempt::new(4).expect("nonzero attempt"),
    );
    assert_eq!(finished.provider_attempt.get(), 4);
    assert_eq!(finished.usage, Some(usage));
    let cache = finished
        .usage
        .and_then(|usage| usage.cache)
        .expect("cache usage");
    assert_eq!(cache.read_tokens, Some(80));
    assert_eq!(cache.write_tokens, Some(20));
}

/// Ensures TRACE prompt diagnostics expose only fixed structural metadata,
/// never model-visible prompt content that belongs in separately gated private
/// capture.
#[test]
fn provider_prompt_trace_omits_model_visible_content() {
    let mut prompt = minimal_prompt();
    let image_bytes = b"trace-image-bytes-sentinel";
    prompt.system_prompt = "trace-system-prompt-sentinel".to_owned();
    prompt.context = tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![
                    tau_proto::ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: "trace-transcript-sentinel".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: Some("trace-raw-transcript-sentinel".to_owned()),
                    }),
                    tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
                        call_id: "trace-call-id".into(),
                        name: tau_proto::ToolName::new("trace_tool_call"),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: tau_proto::CborValue::Text(
                            "trace-tool-arguments-sentinel".to_owned(),
                        ),
                        raw_arguments_json: Some("trace-raw-arguments-sentinel".to_owned()),
                        responses_envelope: None,
                    }),
                ],
            }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    call_id: "trace-call-id".into(),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                        "trace-tool-result-sentinel".to_owned(),
                    )),
                    presentation: Default::default(),
                    provider_content: vec![tau_proto::ToolResultContentPart::Image(
                        tau_proto::ImageContent {
                            media_type: tau_proto::ImageMediaType::Png,
                            data: image_bytes.to_vec().into(),
                            width: 1,
                            height: 1,
                            detail: tau_proto::ImageDetail::High,
                        },
                    )],
                }],
            }),
        ],
    };
    prompt.tools = vec![tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("trace_tool_schema"),
        model_visible_name: None,
        description: Some("trace-tool-description-sentinel".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "trace-tool-schema-sentinel": {"type": "string"},
        })),
        format: None,
    }];
    prompt.originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("trace-originator-sentinel").expect("extension"),
        query_id: "trace-originator-query-sentinel".to_owned(),
    };

    let mut input = Vec::new();
    {
        let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
        writer
            .write_message(&tau_proto::HarnessOutputMessage::Configure(
                tau_proto::Configure {
                    tool_prefix: None,
                    config: tau_proto::CborValue::Map(Vec::new()),
                    instance_name: tau_proto::ExtensionName::parse("provider-builtin")
                        .expect("extension name"),
                    state_dir: None,
                    secrets: BTreeMap::new(),
                    settings_files: BTreeMap::new(),
                },
            ))
            .expect("encode Configure");
        writer
            .write_message(&tau_proto::HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(1),
                tau_proto::Event::AgentPromptCreated(prompt),
            ))
            .expect("encode provider prompt");
        writer.flush().expect("flush harness input");
    }

    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let _ = run(path_std_io::Cursor::new(input.clone()), Vec::new());
    });

    let trace = String::from_utf8(trace.bytes()).expect("TRACE output is UTF-8");
    let image_json = serde_json::to_string(image_bytes).expect("serialize image bytes");
    let image_pretty_json =
        serde_json::to_string_pretty(image_bytes).expect("pretty serialize image bytes");
    let sentinels = [
        "trace-system-prompt-sentinel".to_owned(),
        "trace-transcript-sentinel".to_owned(),
        "trace-raw-transcript-sentinel".to_owned(),
        "trace-tool-arguments-sentinel".to_owned(),
        "trace-raw-arguments-sentinel".to_owned(),
        "trace-tool-result-sentinel".to_owned(),
        "trace-image-bytes-sentinel".to_owned(),
        image_json,
        image_pretty_json,
        "trace_tool_schema".to_owned(),
        "trace-tool-description-sentinel".to_owned(),
        "trace-tool-schema-sentinel".to_owned(),
        "trace-originator-sentinel".to_owned(),
        "trace-originator-query-sentinel".to_owned(),
    ];
    for sentinel in &sentinels {
        assert!(
            !trace.contains(sentinel),
            "{sentinel} leaked in TRACE: {trace}"
        );
    }
    let prompt_trace_lines: Vec<_> = trace
        .lines()
        .filter(|line| line.contains("provider prompt received; content omitted"))
        .collect();
    assert_eq!(prompt_trace_lines.len(), 1, "TRACE output: {trace}");
    assert!(
        prompt_trace_lines[0].contains(
            "agent_prompt_id=ap-test system_prompt_present=true context_blocks=2 \
             context_items=3 tools=1 tools_ref_present=false"
        ),
        "TRACE fields changed or are not fixed: {}",
        prompt_trace_lines[0]
    );
    let receipt_lines: Vec<_> = trace
        .lines()
        .filter(|line| line.contains("provider receipt observation"))
        .collect();
    assert_eq!(receipt_lines.len(), 1, "TRACE output: {trace}");
    let receipt = receipt_lines[0];
    for sentinel in sentinels
        .iter()
        .map(String::as_str)
        .chain(["ap-test", "test", "default"])
    {
        assert!(
            !receipt.contains(sentinel),
            "{sentinel} leaked in receipt TRACE: {receipt}"
        );
    }
    let keys = receipt
        .split_whitespace()
        .filter_map(|field| field.split_once('=').map(|(key, _)| key))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "frame_bytes",
            "frame_read_decode_us",
            "reader_queue_us",
            "handler_clone_us",
            "handler_dispatch_us",
            "settings_clone_us",
            "profile_count",
            "secret_rpc_count",
            "secret_bytes",
            "secret_wait_us",
            "oauth_class",
            "oauth_us",
            "quota_us",
            "cooldown_queue_us",
            "cooldown_depth",
            "slot_queue_us",
            "slot_depth",
            "spawn_us",
            "stage_accounted_us",
            "unattributed_us",
            "receipt_to_worker_us",
            "outcome",
        ],
        "receipt schema changed: {receipt}"
    );
    assert!(
        !receipt.contains("frame_bytes=0"),
        "real decoder size was not propagated: {receipt}"
    );

    let receipt_only = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.receipt=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let receipt_only = receipt_only.clone();
            move || receipt_only.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let _ = run(path_std_io::Cursor::new(input), Vec::new());
    });
    let receipt_only =
        String::from_utf8(receipt_only.bytes()).expect("receipt-only TRACE output is UTF-8");
    let receipt_only_lines = receipt_only.lines().collect::<Vec<_>>();
    assert_eq!(receipt_only_lines.len(), 1, "TRACE output: {receipt_only}");
    assert!(receipt_only.contains("provider receipt observation"));
    assert!(!receipt_only.contains("ap-test"));
    assert!(!receipt_only.contains("provider prompt received"));
}

/// A real zero-capacity provider runtime must carry the receipt observation
/// through slot admission and close the queued owner exactly once on
/// disconnect.
#[test]
fn receipt_trace_observes_real_worker_slot_queue() {
    let mut input = Vec::new();
    {
        let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
        writer
            .write_message(&tau_proto::HarnessOutputMessage::Configure(
                tau_proto::Configure {
                    tool_prefix: None,
                    config: tau_proto::CborValue::Map(Vec::new()),
                    instance_name: tau_proto::ExtensionName::parse("provider-builtin")
                        .expect("extension name"),
                    state_dir: None,
                    secrets: BTreeMap::new(),
                    settings_files: BTreeMap::new(),
                },
            ))
            .expect("encode Configure");
        writer
            .write_message(&tau_proto::HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(1),
                tau_proto::Event::AgentPromptCreated(minimal_prompt()),
            ))
            .expect("encode prompt");
        writer
            .write_message(&tau_proto::HarnessOutputMessage::Disconnect(
                tau_proto::Disconnect {
                    reason: Some("private-disconnect-canary".to_owned()),
                },
            ))
            .expect("encode disconnect");
        writer.flush().expect("flush input");
    }
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.receipt=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        run_inner_with_executors(
            path_std_io::Cursor::new(input),
            Vec::new(),
            BuiltinProviderProfiles::default(),
            |_| BuiltinProviderProfiles::default(),
            0,
            Arc::new(|_| panic!("zero-capacity runtime started a worker")),
            production_prewarm_executor(),
        )
        .expect("run queued provider");
    });
    let trace = String::from_utf8(trace.bytes()).expect("UTF-8 trace");
    let receipt = trace
        .lines()
        .find(|line| line.contains("provider receipt observation"))
        .expect("queued receipt trace");

    assert!(receipt.contains("slot_depth=1"), "{receipt}");
    assert!(receipt.contains("outcome=\"failed\""), "{receipt}");
    assert!(!receipt.contains("private-disconnect-canary"), "{receipt}");
    assert_eq!(trace.matches("provider receipt observation").count(), 1);
}

fn decode_frames(bytes: &[u8]) -> Vec<tau_proto::HarnessInputMessage> {
    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
}

/// Ensures the built-in ChatGPT/Codex emission boundary does not suppress
/// public stats-only streams that have no displayable text or compaction.
#[test]
fn chatgpt_stream_update_emits_response_stats_without_text_deltas() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    tau_provider_codex::test_append_custom_tool_input(&mut state, 0, "raw custom input");
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut delta_emitter = CodexStreamDeltaEmitter::default();
        let parsed_prompt_id = tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
            .expect("test prompt id");
        let target = ResponseUpdateTarget {
            agent_prompt_id: &parsed_prompt_id,
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        emit_chatgpt_stream_update(
            &target,
            &state,
            &mut delta_emitter,
            ProviderResponseStats {
                current: tau_proto::ProviderResponseStatsSample {
                    response_bytes_received: state.response_bytes_received(),
                    elapsed_micros: 1_000_000,
                },
                previous: tau_proto::ProviderResponseStatsSample::default(),
                first_semantic_output_elapsed_micros: None,
            },
            None,
            &mut writer,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(tau_proto::HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected provider response update frame: {frames:?}");
    };
    let tau_proto::Event::ProviderResponseUpdatedReported(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(update.deltas.is_empty());
    assert_eq!(
        update
            .response_stats
            .as_ref()
            .map(|stats| stats.current.response_bytes_received),
        Some("raw custom input".len() as u64),
    );
}

/// Fresh WebSocket work must expose only a fixed, content-free connecting
/// status before any provider request or response bytes are available.
#[test]
fn chatgpt_connecting_update_is_sanitized() {
    let prompt = minimal_prompt();
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_chatgpt_connecting_update(
            &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            &prompt.agent_id,
            &prompt.originator,
            &mut writer,
        );
    }
    let frames = decode_frames(&bytes);
    assert_eq!(frames.len(), 1, "connecting emits exactly one frame");
    let Some(tau_proto::HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected connecting status frame: {frames:?}");
    };
    let tau_proto::Event::ProviderResponseUpdatedReported(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(update.deltas.is_empty());
    assert!(update.compaction.is_none());
    assert!(update.response_stats.is_none());
    assert!(matches!(
        &update.status,
        Some(tau_proto::ProviderResponseStatusUpdate {
            text,
            clear_response: false,
            retry: None,
                    native_tool: None,
        }) if text == "Connecting to provider…"
    ));
}

/// Ensures ChatGPT/Codex provider progress frames publish the first streamed
/// chunk promptly, then follow provider-prompt cadence instead of emitting once
/// per upstream chunk or byte change.
#[test]
fn chatgpt_response_update_emitter_rate_limits_non_terminal_updates() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        tau_provider_codex::test_append_message_delta(&mut state, 0, "hel");
        emitter.emit_at(&target, &state, &mut writer, start, false);
        tau_provider_codex::test_append_message_delta(&mut state, 0, "lo");
        tau_provider_codex::test_append_custom_tool_input(&mut state, 1, "raw custom input");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "updates: {updates:#?}");
    assert_eq!(
        updates[0].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "hel".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[0].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: "hel".len() as u64,
                elapsed_micros: 0,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 0,
            },
            first_semantic_output_elapsed_micros: Some(0),
        })
    );
    assert_eq!(
        updates[1].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "lo".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[1].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: ("hello".len() + "raw custom input".len()) as u64,
                elapsed_micros: 1_000_000,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: "hel".len() as u64,
                elapsed_micros: 0,
            },
            first_semantic_output_elapsed_micros: Some(0),
        })
    );
}

/// Hosted search lifecycle transitions must bypass ordinary stream cadence and
/// retain the provider call id in typed transient native-tool status.
#[test]
fn chatgpt_response_update_emitter_publishes_typed_native_web_search() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        tau_provider_codex::test_set_web_search_active(&mut state, 0, "ws_1", true);
        emitter.emit_at(&target, &state, &mut writer, start, false);
        tau_provider_codex::test_set_web_search_active(&mut state, 0, "ws_1", false);
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
    }

    let updates = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 2);
    for (update, phase, status) in [
        (
            &updates[0],
            tau_proto::ProviderNativeToolPhase::Started,
            tau_proto::ToolUseStatus::InProgress,
        ),
        (
            &updates[1],
            tau_proto::ProviderNativeToolPhase::Completed,
            tau_proto::ToolUseStatus::Success,
        ),
    ] {
        let native = update
            .status
            .as_ref()
            .and_then(|status| status.native_tool.as_ref())
            .expect("typed native tool update");
        assert_eq!(native.call_id, "ws_1");
        assert_eq!(native.tool_name.as_str(), "web_search");
        assert_eq!(native.phase, phase);
        assert_eq!(native.display.status, status);
    }
}

/// Overlapping hosted searches must publish each per-call transition even when
/// the aggregate searching/not-searching state does not change.
#[test]
fn chatgpt_response_update_emitter_preserves_overlapping_native_searches() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        for (index, call_id, active) in [
            (0, "ws_a", true),
            (1, "ws_b", true),
            (0, "ws_a", false),
            (1, "ws_b", false),
        ] {
            tau_provider_codex::test_set_web_search_active(&mut state, index, call_id, active);
            emitter.emit_at(&target, &state, &mut writer, start, false);
        }
    }

    let lifecycles = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => update
                    .status
                    .and_then(|status| status.native_tool)
                    .map(|native| (native.call_id, native.phase)),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycles,
        vec![
            (
                "ws_a".to_owned(),
                tau_proto::ProviderNativeToolPhase::Started
            ),
            (
                "ws_b".to_owned(),
                tau_proto::ProviderNativeToolPhase::Started
            ),
            (
                "ws_a".to_owned(),
                tau_proto::ProviderNativeToolPhase::Completed
            ),
            (
                "ws_b".to_owned(),
                tau_proto::ProviderNativeToolPhase::Completed
            ),
        ]
    );
}

/// Codex captures semantic onset even when its callback is cadence-suppressed,
/// repeats the value at terminal flush, and a fresh finite attempt starts
/// empty.
#[test]
fn chatgpt_first_output_capture_survives_batching_flush_and_attempt_reset() {
    let prompt = minimal_prompt();
    let start = path_std_time::Instant::now();
    let target = ResponseUpdateTarget {
        agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
            .expect("test prompt id"),
        agent_id: &prompt.agent_id,
        originator: &prompt.originator,
    };
    let mut state = tau_provider_codex::test_stream_state();
    tau_provider_codex::test_record_transport_response_bytes(&mut state, 1);
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        emitter.emit_at(&target, &state, &mut writer, start, false);
        tau_provider_codex::test_append_message_delta(&mut state, 0, "hello");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL * 2,
            true,
        );

        let fresh_state = tau_provider_codex::test_stream_state();
        let mut fresh = RateLimitedResponseUpdateEmitter::new_at(start);
        fresh.emit_at(
            &target,
            &fresh_state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            true,
        );
    }
    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 4);
    assert_eq!(
        updates[0]
            .response_stats
            .as_ref()
            .expect("transport stats")
            .first_semantic_output_elapsed_micros,
        None
    );
    for update in &updates[1..3] {
        assert_eq!(
            update
                .response_stats
                .as_ref()
                .expect("semantic stats")
                .first_semantic_output_elapsed_micros,
            Some(500_000)
        );
    }
    assert_eq!(
        updates[3]
            .response_stats
            .as_ref()
            .expect("fresh-attempt stats")
            .first_semantic_output_elapsed_micros,
        None
    );
}

/// Ensures due ChatGPT/Codex response samples are emitted even when no bytes
/// changed, so provider `previous` always names the last emitted stats point.
#[test]
fn chatgpt_response_update_emitter_emits_due_stats_only_sample() {
    let prompt = minimal_prompt();
    let state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL * 2,
            false,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "updates: {updates:#?}");
    assert!(updates.iter().all(|update| update.deltas.is_empty()));
    assert_eq!(
        updates[0].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 1_000_000,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 0,
            },
            first_semantic_output_elapsed_micros: None,
        })
    );
    assert_eq!(
        updates[1].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 2_000_000,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 1_000_000,
            },
            first_semantic_output_elapsed_micros: None,
        })
    );
}

/// Ensures a due zero-byte idle sample does not consume the first non-empty
/// bypass for streamed output, while later non-terminal bytes still obey the
/// one-second cadence.
#[test]
fn chatgpt_response_update_emitter_emits_first_bytes_after_idle_sample_promptly() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
        tau_provider_codex::test_append_message_delta(&mut state, 0, "hi");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        tau_provider_codex::test_append_message_delta(&mut state, 0, "!");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 4,
            false,
        );
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 3, "updates: {updates:#?}");
    assert!(updates[0].deltas.is_empty());
    assert_eq!(
        updates[1].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "hi".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[1].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: "hi".len() as u64,
                elapsed_micros: 1_500_000,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: 0,
                elapsed_micros: 1_000_000,
            },
            first_semantic_output_elapsed_micros: Some(1_500_000),
        })
    );
    assert_eq!(
        updates[2].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "!".to_owned(),
            phase: None,
        }]
    );
}

/// Ensures the first non-empty progress bypass applies to stats-only tool input
/// bytes, not just visible assistant text.
#[test]
fn chatgpt_response_update_emitter_emits_first_stats_only_sample_promptly() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        tau_provider_codex::test_append_custom_tool_input(&mut state, 1, "raw custom input");
        emitter.emit_at(&target, &state, &mut writer, start, false);
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 1, "updates: {updates:#?}");
    assert!(updates[0].deltas.is_empty());
    assert_eq!(
        updates[0]
            .response_stats
            .as_ref()
            .expect("stats-only update should carry provider stats")
            .current
            .response_bytes_received,
        "raw custom input".len() as u64
    );
}

/// Ensures a terminal flush can publish the final suffix immediately before
/// `provider.response_finished`, without losing text suppressed by the
/// non-terminal one-second cadence after the first streamed chunk.
#[test]
fn chatgpt_response_update_emitter_terminal_flush_emits_batched_suffix() {
    let prompt = minimal_prompt();
    let mut state = tau_provider_codex::test_stream_state();
    let mut bytes = Vec::new();
    let start = path_std_time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        tau_provider_codex::test_append_message_delta(&mut state, 0, "hel");
        emitter.emit_at(&target, &state, &mut writer, start, false);
        tau_provider_codex::test_append_message_delta(&mut state, 0, "lo");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            true,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdatedReported(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "updates: {updates:#?}");
    assert_eq!(
        updates[0].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "hel".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[0]
            .response_stats
            .as_ref()
            .expect("initial update should carry provider stats")
            .current
            .response_bytes_received,
        "hel".len() as u64
    );
    assert_eq!(
        updates[1].deltas,
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "lo".to_owned(),
            phase: None,
        }]
    );
    assert_eq!(
        updates[1]
            .response_stats
            .expect("terminal flush should carry provider stats")
            .current
            .response_bytes_received,
        "hello".len() as u64
    );
}

/// A built-in repetition error must clear transient output and finish with an
/// empty non-retryable response rather than preserving repeated stream text.
#[test]
fn chatgpt_repetition_error_uses_clear_response_and_empty_final_output() {
    let prompt = minimal_prompt();
    let repetition = tau_provider::StreamRepetition {
        key: tau_provider::StreamRepetitionKey::AssistantText { output_index: 0 },
        mode: tau_provider::RepetitionMode::Fragment,
        snippet: ".".to_owned(),
    };
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_repetition_detected_update(
            &tau_proto::AgentPromptId::parse("ap-test").expect("test prompt id"),
            &prompt.agent_id,
            &prompt.originator,
            &repetition,
            &mut writer,
        );
    }
    let frames = decode_frames(&bytes);
    let Some(tau_proto::HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected repetition status frame: {frames:?}");
    };
    let tau_proto::Event::ProviderResponseUpdatedReported(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(matches!(
        &update.status,
        Some(tau_proto::ProviderResponseStatusUpdate {
            clear_response: true,
            text,
            ..
        }) if text.contains("repetition detected")
    ));

    let backend = tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::Responses,
        base_url: "https://example.invalid".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    };
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        finish_error(
            &tau_proto::AgentPromptId::parse("ap-test").expect("test prompt id"),
            &prompt,
            Some(&backend),
            tau_provider_codex::CodexError::from_repetition(repetition),
            None,
            false,
            tau_proto::ProviderAttempt::ONE,
            &mut writer,
        )
        .expect("finish repetition error");
    }
    let frames = decode_frames(&bytes);
    let Some(tau_proto::HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected provider response finished frame: {frames:?}");
    };
    let tau_proto::Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
        panic!("expected provider response finished: {:?}", emit.event);
    };
    assert_eq!(
        finished.stop_reason,
        tau_proto::ProviderStopReason::RepetitionDetected
    );
    assert!(finished.output_items.is_empty());
    assert!(finished.error.as_deref().unwrap_or_default().len() <= 520);
}
/// Full reconciliation preserves a rolling window accepted after fetch start,
/// while still deleting older pools absent from the full account response.
#[test]
fn quota_reconciliation_does_not_revert_newer_rolling_state() {
    let provider = ProviderName::new("chatgpt");
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let mut quota = QuotaCoordinator::default();
    let established = quota
        .ensure_profile(provider.clone(), 7)
        .expect("valid quota test value");
    assert!(matches!(
        established,
        Event::ProviderQuotaReplaceReported(_)
    ));
    let (epoch, fetch_sequence) = quota
        .begin_fetch(&provider)
        .expect("valid quota test value");
    let rolling = tau_provider_codex::RollingQuotaObservation {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                .expect("valid quota test value"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                .expect("valid quota test value"),
            used_basis_points: 6_000,
            window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
            reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
            remaining_seconds: None,
        }],
        active_limit_id: Some(
            tau_proto::ProviderQuotaLimitId::parse("codex").expect("valid quota test value"),
        ),
        binding_provenance: Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent),
    };
    assert!(matches!(
        quota.merge_rolling(model, 7, rolling, UnixMillis::new(2_000_000_000_000)),
        Some(Event::ProviderQuotaPatchReported(_))
    ));
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                .expect("valid quota test value"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                .expect("valid quota test value"),
            used_basis_points: 5_000,
            window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
            reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
            remaining_seconds: Some(tau_proto::SignedSeconds::new(500_000)),
        }],
    };
    let Event::ProviderQuotaReplaceReported(replaced) = quota
        .finish_fetch(
            provider,
            epoch,
            fetch_sequence,
            full,
            UnixMillis::new(2_000_000_001_000),
        )
        .expect("valid quota test value")
    else {
        panic!("expected replacement");
    };
    assert_eq!(replaced.windows[0].used_basis_points, 6_000);
    assert_eq!(replaced.route_bindings.len(), 1);
}

/// First-party replace, patch, and coordinator-generated clear observations use
/// their report variants with explicit `persist=false` metadata.
#[test]
fn quota_report_messages_use_persist_false_for_every_operation() {
    let provider = ProviderName::new("chatgpt");
    let mut quota = QuotaCoordinator::default();
    let replace = quota
        .ensure_profile(provider.clone(), 7)
        .expect("establish quota profile");
    let epoch = quota.profile_epoch(&provider).expect("profile epoch");
    let patch = Event::ProviderQuotaPatchReported(tau_proto::ProviderQuotaPatch {
        provider: provider.clone(),
        profile_epoch: epoch,
        sequence: tau_proto::ProviderQuotaSequence::new(2),
        windows: Vec::new(),
        removed_window_keys: Vec::new(),
        route_bindings: Vec::new(),
    });
    let clear = quota.clear_profile(&provider).expect("clear quota profile");

    for (event, expected_name) in [
        (
            replace,
            tau_proto::EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
        ),
        (patch, tau_proto::EventName::PROVIDER_QUOTA_PATCH_REPORTED),
        (clear, tau_proto::EventName::PROVIDER_QUOTA_CLEAR_REPORTED),
    ] {
        let HarnessInputMessage::Emit(emit) = quota_report_message(event) else {
            panic!("quota helper must produce Emit");
        };
        assert!(!emit.persist);
        assert_eq!(emit.event.name(), expected_name);
    }
}

/// The real two-pool account shape remains unbound after full reconciliation,
/// then an official nameless turn event binds only the exact model to default
/// `codex` without accidentally selecting the additional Bengalfox pool.
#[test]
fn quota_two_pool_snapshot_then_nameless_turn_binds_default_pool() {
    let provider = ProviderName::new("chatgpt");
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let mut quota = QuotaCoordinator::default();
    quota.ensure_profile(provider.clone(), 7);
    let (epoch, fetch_sequence) = quota.begin_fetch(&provider).expect("quota fetch");
    let window = |limit_id: &str, used_basis_points| tau_provider_codex::QuotaWindowObservation {
        limit_id: tau_proto::ProviderQuotaLimitId::parse(limit_id).expect("pool id"),
        window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
        used_basis_points,
        window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
        reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
        remaining_seconds: Some(tau_proto::SignedSeconds::new(500_000)),
    };
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: vec![window("codex", 4_400), window("codex_bengalfox", 0)],
    };
    let Event::ProviderQuotaReplaceReported(replaced) = quota
        .finish_fetch(
            provider,
            epoch,
            fetch_sequence,
            full,
            UnixMillis::new(2_000_000_000_000),
        )
        .expect("full quota replacement")
    else {
        panic!("expected quota replacement");
    };
    assert_eq!(replaced.windows.len(), 2);
    assert!(replaced.route_bindings.is_empty());

    let observation = tau_provider_codex::parse_quota_ws_event(
        r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":45,"window_minutes":10080,"reset_at":2100000000}}}"#,
    )
    .expect("official nameless turn event");
    let Event::ProviderQuotaPatchReported(patch) = quota
        .merge_rolling(
            model.clone(),
            7,
            observation,
            UnixMillis::new(2_000_000_001_000),
        )
        .expect("quota binding patch")
    else {
        panic!("expected quota patch");
    };
    assert_eq!(patch.route_bindings.len(), 1);
    assert_eq!(patch.route_bindings[0].model, model);
    assert_eq!(patch.route_bindings[0].limit_ids[0].as_str(), "codex");
    assert!(
        patch.route_bindings[0]
            .limit_ids
            .iter()
            .all(|id| id.as_str() != "codex_bengalfox")
    );
}

/// An old account fetch can never repopulate quota state after a profile epoch
/// rotates, even when its network response arrives later.
#[test]
fn quota_profile_rotation_rejects_old_fetch_completion() {
    let provider = ProviderName::new("chatgpt");
    let mut quota = QuotaCoordinator::default();
    quota.ensure_profile(provider.clone(), 1);
    let (old_epoch, sequence) = quota
        .begin_fetch(&provider)
        .expect("valid quota test value");
    quota.ensure_profile(provider.clone(), 2);
    assert!(
        quota
            .finish_fetch(
                provider,
                old_epoch,
                sequence,
                tau_provider_codex::FullQuotaSnapshot::default(),
                UnixMillis::new(1),
            )
            .is_none()
    );
}

/// Sparse rolling observations cannot grow the coordinator beyond the protocol
/// state bound; the rejected update is atomic and consumes no sequence.
#[test]
fn quota_sparse_state_is_bounded_before_mutation() {
    let provider = ProviderName::new("chatgpt");
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let mut quota = QuotaCoordinator::default();
    quota.ensure_profile(provider.clone(), 7);
    for index in 0..tau_proto::MAX_PROVIDER_QUOTA_WINDOWS {
        let observation = tau_provider_codex::RollingQuotaObservation {
            windows: vec![tau_provider_codex::QuotaWindowObservation {
                limit_id: tau_proto::ProviderQuotaLimitId::parse(format!("pool_{index}"))
                    .expect("pool id"),
                window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
                used_basis_points: 100,
                window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
                reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
                remaining_seconds: None,
            }],
            active_limit_id: None,
            binding_provenance: None,
        };
        assert!(
            quota
                .merge_rolling(
                    model.clone(),
                    7,
                    observation,
                    UnixMillis::new(2_000_000_000_000)
                )
                .is_some()
        );
    }
    let sequence = quota.profiles[&provider].sequence;
    let overflow = tau_provider_codex::RollingQuotaObservation {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("overflow").expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
            used_basis_points: 100,
            window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
            reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
            remaining_seconds: None,
        }],
        active_limit_id: None,
        binding_provenance: None,
    };
    assert!(
        quota
            .merge_rolling(model, 7, overflow, UnixMillis::new(2_000_000_000_001))
            .is_none()
    );
    assert_eq!(quota.profiles[&provider].sequence, sequence);
    assert_eq!(
        quota.profiles[&provider].windows.len(),
        tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
    );
}

/// Full reconciliation validates the post-race merged candidate atomically;
/// fetched keys cannot overflow the bound alongside post-start rolling keys.
#[test]
fn quota_full_merge_with_post_start_keys_cannot_overflow_bound() {
    let provider = ProviderName::new("chatgpt");
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let mut quota = QuotaCoordinator::default();
    quota.ensure_profile(provider.clone(), 7);
    let rolling = |prefix: &str, index: usize| tau_provider_codex::RollingQuotaObservation {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse(format!("{prefix}_{index}"))
                .expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
            used_basis_points: 100,
            window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
            reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
            remaining_seconds: None,
        }],
        active_limit_id: None,
        binding_provenance: None,
    };
    for index in 0..16 {
        quota.merge_rolling(
            model.clone(),
            7,
            rolling("old", index),
            UnixMillis::new(2_000_000_000_000),
        );
    }
    let (epoch, fetch_sequence) = quota.begin_fetch(&provider).expect("fetch");
    for index in 0..16 {
        quota.merge_rolling(
            model.clone(),
            7,
            rolling("new", index),
            UnixMillis::new(2_000_000_000_001),
        );
    }
    let sequence = quota.profiles[&provider].sequence;
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: (0..32)
            .map(|index| tau_provider_codex::QuotaWindowObservation {
                limit_id: tau_proto::ProviderQuotaLimitId::parse(format!("full_{index}"))
                    .expect("pool id"),
                window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
                used_basis_points: 200,
                window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
                reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
                remaining_seconds: Some(tau_proto::SignedSeconds::new(300_000)),
            })
            .collect(),
    };
    assert!(
        quota
            .finish_fetch(
                provider.clone(),
                epoch,
                fetch_sequence,
                full,
                UnixMillis::new(2_000_000_000_002),
            )
            .is_none()
    );
    assert_eq!(quota.profiles[&provider].sequence, sequence);
    assert_eq!(
        quota.profiles[&provider].windows.len(),
        tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
    );
}

/// Refresh deadlines are generation-coalesced per epoch and failures advance a
/// bounded backoff instead of creating parallel permanent polling chains.
#[test]
fn quota_refresh_deadlines_coalesce_and_back_off() {
    let provider = ProviderName::new("chatgpt");
    let mut quota = QuotaCoordinator::default();
    quota.ensure_profile(provider.clone(), 7);
    let epoch = quota.profile_epoch(&provider).expect("epoch");
    let first = quota
        .schedule_refresh(&provider, &epoch)
        .expect("generation");
    let second = quota
        .schedule_refresh(&provider, &epoch)
        .expect("generation");
    assert!(!quota.refresh_is_current(&provider, &epoch, first));
    assert!(quota.refresh_is_current(&provider, &epoch, second));
    let _ = quota.begin_fetch(&provider).expect("fetch");
    quota.fail_fetch(&provider, &epoch);
    assert!(quota.failure_delay(&provider) > QUOTA_FETCH_MIN_INTERVAL);
    for _ in 0..10 {
        quota.fail_fetch(&provider, &epoch);
    }
    assert_eq!(quota.failure_delay(&provider), QUOTA_REFRESH_INTERVAL);
    quota.ensure_profile(provider.clone(), 8);
    assert!(!quota.refresh_is_current(&provider, &epoch, second));
}
