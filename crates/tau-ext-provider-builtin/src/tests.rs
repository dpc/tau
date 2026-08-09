use std::fs::File;
use std::sync::atomic as path_std_sync_atomic;
use std::{io as path_std_io, time as path_std_time};

use tau_provider_codex::oauth as path_tau_provider_codex_oauth;

mod compatibility;

use super::*;

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

fn cli_setup_plan(name: &str) -> setup_store::ProviderSetupPlan {
    let provider = ProviderName::try_new(name.to_owned()).expect("provider");
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
                "secret_path": format!("providers/{provider}/api-key.json")
            }
        }))
        .expect("settings"),
        credential: setup_store::CredentialSetup::Stored {
            secret: setup_store::SecretWrite {
                path: ProviderCredentialSlot::ApiKey.path(&provider),
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
    let store = setup_store::SetupStore::open_in(temp.path());
    let config_profiles = temp.path().join("config/providers/provider-builtin");
    let state_profiles = temp.path().join("providers/provider-builtin");
    std::fs::create_dir_all(&config_profiles).expect("config root");
    std::fs::create_dir_all(&state_profiles).expect("state root");
    let config_count = tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILES / 2;
    for index in 0..config_count {
        std::fs::write(config_profiles.join(format!("c-{index}.json")), b"{}").expect("profile");
    }
    for index in 0..=(tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILES - config_count) {
        std::fs::write(state_profiles.join(format!("s-{index}.json")), b"{}").expect("profile");
    }
    let extension = tau_proto::ExtensionName::parse("provider-builtin").expect("extension");

    let error = cmd_list_from_store(&[], &extension, &store, &mut Vec::new())
        .expect_err("too many profiles");

    let error = error.to_string();
    assert!(error.contains("state profile discovery"));
    assert!(error.contains("exceeds"));
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

    write_dotfiles_profile(settings, true, &mut stdout, &mut stderr).expect("output");

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
    let provider = ProviderName::new("status");
    let key = (provider.clone(), ProviderCredentialSlot::ApiKey);
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
        assert_eq!(setup_api_key_status(&credentials, &provider), expected);
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

fn configured_chat_completions_settings(provider: &str, extra: serde_json::Value) -> Vec<u8> {
    let mut settings = serde_json::json!({
        "kind": "chat_completions",
        "models": [{"id": "deepseek-chat"}],
        "credential": {
            "kind": "api_key",
            "secret_path": format!("providers/{provider}/api-key.json")
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
        serde_json::json!({"api_key_secret": "legacy-secret-name"}),
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
            "provider profile 'deepseek' has invalid credential-free settings: credential fields are forbidden"
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

/// Proves a valid complete settings snapshot still publishes every configured
/// model and reaches Ready after strict validation.
#[test]
fn valid_provider_configure_publishes_models_and_ready() {
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
                        if declaration
                            .models
                            .iter()
                            .any(|model| {
                                model.id.provider.as_str() == "deepseek"
                                    && model.id.model.as_str() == "deepseek-chat"
                            })
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

/// Cloneable in-memory sink used to inspect structured tracing output.
#[derive(Clone, Default)]
struct SharedTraceWriter {
    /// Bytes written by the temporary tracing subscriber.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTraceWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("trace writer lock").clone()
    }
}

impl Write for SharedTraceWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace writer lock")
            .extend_from_slice(buffer);
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

#[test]
fn compaction_output_finishes_as_normal_end_turn() {
    // Regression: server-side compaction is now represented by a durable output
    // item, not a special provider lifecycle stop reason.
    let output_items = [tau_proto::ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::new(tau_proto::CborValue::Map(Vec::new())),
    )];

    assert_eq!(
        stop_reason_from_output_items(&output_items),
        tau_proto::ProviderStopReason::EndTurn
    );
}

#[test]
fn compaction_with_tool_calls_still_requests_tools() {
    // Compaction can be returned alongside normal model output. Tool calls still
    // own the provider stop reason so the harness runs them instead of treating
    // the turn as a plain completed end turn.
    let output_items = [
        tau_proto::ContextItem::Compaction(tau_proto::OpaqueProviderItem::new(
            tau_proto::CborValue::Map(Vec::new()),
        )),
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

#[test]
fn synthetic_provider_error_is_not_output_item() {
    // Regression: runtime/provider setup errors are display strings, not
    // assistant messages that should be replayed as future context.
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

#[test]
fn login_subcommand_is_not_part_of_provider_registry_cli() {
    // Registration is intentionally centered on `tau provider add`; ChatGPT
    // OAuth happens as part of adding or replacing that provider profile.
    let args = vec!["login".to_owned(), "chatgpt".to_owned()];

    let error = run_provider_cli(&args).expect_err("login subcommand should fail");

    assert!(
        error
            .to_string()
            .contains("unknown provider subcommand: login")
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

#[test]
fn profile_storage_kinds_do_not_carry_openai_prefix() {
    // Profile files are builtin-provider registrations, not OpenAI account
    // records. Keep the storage tags aligned with the builtin backend kind.
    let chatgpt = serde_json::to_value(BuiltinProviderProfile::Chatgpt(ChatGptProfile::default()))
        .expect("serialize chatgpt profile");
    let chat_completions = serde_json::to_value(BuiltinProviderProfile::ChatCompletions(
        ChatCompletionsProvider::default(),
    ))
    .expect("serialize chat completions profile");
    let openrouter = serde_json::to_value(BuiltinProviderProfile::OpenRouter(
        OpenRouterProfile::default(),
    ))
    .expect("serialize openrouter profile");
    let responses = serde_json::to_value(BuiltinProviderProfile::Responses(
        ResponsesProvider::default(),
    ))
    .expect("serialize responses profile");

    assert_eq!(chatgpt["kind"], "chatgpt");
    assert_eq!(chat_completions["kind"], "chat_completions");
    assert_eq!(openrouter["kind"], "openrouter");
    assert_eq!(responses["kind"], "responses");
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
        }) if access_token == "fresh"
    ));
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
        credentials: Default::default(),
        providers: BTreeMap::from([
            (
                first.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile {
                    auth: expired.clone(),
                    responses_lite_compatibility: false,
                }),
            ),
            (
                second.clone(),
                BuiltinProviderProfile::Chatgpt(ChatGptProfile {
                    auth: expired,
                    responses_lite_compatibility: false,
                }),
            ),
            (
                ProviderName::new("router"),
                BuiltinProviderProfile::OpenRouter(OpenRouterProfile::default()),
            ),
        ]),
    };

    let published = models_for_profiles(&profiles);
    let mut attempts = Vec::new();
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
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
                &mut profile.auth,
                mode,
                &mut refresh_rejections,
                |provider, _, _| {
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
    let provider = ProviderName::new("chatgpt");
    let model = ModelId::new(provider.clone(), ModelName::new("gpt-5.4"));
    let rejection = path_tau_provider_codex_oauth::OAuthError::from_http_response(
        400,
        r#"{"error":{"code":"refresh_token_reused"}}"#,
    );

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
            &mut auth,
            CodexMode::Standard,
            &mut cache,
            |_, _, _| {
                Err(RefreshCredentialsError::OAuth {
                    credentials: Box::new(authoritative),
                    error: rejection.clone(),
                })
            },
        );

        assert_eq!(config.is_some(), expected_available);
    }
}

/// Startup-selected modes independently control same-process publication and
/// overwrite later disk edits until restart.
#[test]
fn chatgpt_profile_modes_are_independent_and_startup_stable() {
    let standard = ProviderName::new("standard");
    let lite = ProviderName::new("lite");
    let mut startup = BuiltinProviderProfiles {
        credentials: Default::default(),
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
    assert!(models.iter().all(|model| model.efforts.is_none()));
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
        context_window: 128_000,
        compat: None,
        tags: Vec::new(),
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
        Some(PromptBackend::ChatCompletions { model, .. }) if model.id == configured.id
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
        api_key: "key".to_owned(),
        models: vec![configured.clone()],
    };
    let mut profiles = BuiltinProviderProfiles {
        credentials: Default::default(),
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
        Some(PromptBackend::ChatCompletions { provider, model })
            if provider.base_url == "https://openrouter.ai/api/v1"
                && model.id == configured.id
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

#[test]
fn generated_retry_delay_caps_without_exhausting_attempts() {
    // Persistent failures continue indefinitely while policy-generated cadence
    // reaches, but never exceeds, the approved thirty-minute ceiling.
    let mut state = PromptRetryState::default();
    for _ in 0..10_000 {
        let delay = state.next_delay(RetryClass::Unknown, "ap-persistent");
        assert!(delay <= Duration::from_secs(30 * 60));
    }
    assert_eq!(state.attempts, 10_000);
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

#[test]
fn cancellation_waker_fires_for_matching_prompt_only() {
    // WebSocket turns park on provider events for up to the stream idle
    // watchdog. The cancellation registry must therefore wake the matching turn
    // directly, without relying on periodic receive timeouts.
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

#[test]
fn cancellation_shutdown_wakes_all_registered_abort_wakers() {
    // Provider shutdown must wake every active ChatGPT WebSocket turn so workers
    // can return their normal canceled terminal path instead of waiting on idle
    // upstream sockets.
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

#[test]
fn cancellation_waker_guard_unregisters_on_drop() {
    // Completed turns drop their abort-waker guard. Later cancellation for the
    // same prompt id must not enqueue stale wake hints into a reused socket's
    // inbound event stream.
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
        emit_chatgpt_stream_update(
            &tau_proto::AgentPromptId::parse(prompt.agent_prompt_id.as_str())
                .expect("test prompt id"),
            &prompt.agent_id,
            &prompt.originator,
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

#[test]
fn chatgpt_repetition_error_uses_clear_response_and_empty_final_output() {
    // Built-in ChatGPT/Codex errors from the stream guard clear transient output
    // and finish as a non-retryable repetition-detected provider response.
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
            &backend,
            tau_provider_codex::CodexError::from_repetition(repetition),
            None,
            false,
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
            window_seconds: Some(604_800),
            reset_at_unix_seconds: Some(2_100_000_000),
            remaining_seconds: None,
        }],
        active_limit_id: Some(
            tau_proto::ProviderQuotaLimitId::parse("codex").expect("valid quota test value"),
        ),
        binding_provenance: Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent),
    };
    assert!(matches!(
        quota.merge_rolling(model, 7, rolling, 2_000_000_000_000),
        Some(Event::ProviderQuotaPatchReported(_))
    ));
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                .expect("valid quota test value"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                .expect("valid quota test value"),
            used_basis_points: 5_000,
            window_seconds: Some(604_800),
            reset_at_unix_seconds: Some(2_100_000_000),
            remaining_seconds: Some(500_000),
        }],
    };
    let Event::ProviderQuotaReplaceReported(replaced) = quota
        .finish_fetch(provider, epoch, fetch_sequence, full, 2_000_000_001_000)
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
        sequence: 2,
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
        window_seconds: Some(604_800),
        reset_at_unix_seconds: Some(2_100_000_000),
        remaining_seconds: Some(500_000),
    };
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: vec![window("codex", 4_400), window("codex_bengalfox", 0)],
    };
    let Event::ProviderQuotaReplaceReported(replaced) = quota
        .finish_fetch(provider, epoch, fetch_sequence, full, 2_000_000_000_000)
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
        .merge_rolling(model.clone(), 7, observation, 2_000_000_001_000)
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
                1,
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
                window_seconds: Some(604_800),
                reset_at_unix_seconds: Some(2_100_000_000),
                remaining_seconds: None,
            }],
            active_limit_id: None,
            binding_provenance: None,
        };
        assert!(
            quota
                .merge_rolling(model.clone(), 7, observation, 2_000_000_000_000)
                .is_some()
        );
    }
    let sequence = quota.profiles[&provider].sequence;
    let overflow = tau_provider_codex::RollingQuotaObservation {
        windows: vec![tau_provider_codex::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("overflow").expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
            used_basis_points: 100,
            window_seconds: Some(604_800),
            reset_at_unix_seconds: Some(2_100_000_000),
            remaining_seconds: None,
        }],
        active_limit_id: None,
        binding_provenance: None,
    };
    assert!(
        quota
            .merge_rolling(model, 7, overflow, 2_000_000_000_001)
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
            window_seconds: Some(604_800),
            reset_at_unix_seconds: Some(2_100_000_000),
            remaining_seconds: None,
        }],
        active_limit_id: None,
        binding_provenance: None,
    };
    for index in 0..16 {
        quota.merge_rolling(model.clone(), 7, rolling("old", index), 2_000_000_000_000);
    }
    let (epoch, fetch_sequence) = quota.begin_fetch(&provider).expect("fetch");
    for index in 0..16 {
        quota.merge_rolling(model.clone(), 7, rolling("new", index), 2_000_000_000_001);
    }
    let sequence = quota.profiles[&provider].sequence;
    let full = tau_provider_codex::FullQuotaSnapshot {
        windows: (0..32)
            .map(|index| tau_provider_codex::QuotaWindowObservation {
                limit_id: tau_proto::ProviderQuotaLimitId::parse(format!("full_{index}"))
                    .expect("pool id"),
                window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
                used_basis_points: 200,
                window_seconds: Some(604_800),
                reset_at_unix_seconds: Some(2_100_000_000),
                remaining_seconds: Some(300_000),
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
                2_000_000_000_002,
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
