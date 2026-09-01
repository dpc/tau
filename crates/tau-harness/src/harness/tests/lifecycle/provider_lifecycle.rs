//! Tests for provider lifecycle behavior.

use super::*;
use crate::harness::HarnessSessionLaunchMode;

/// Proves a late opaque capture keeps its Provider-supplied durable session
/// attribution after the harness rolls to a replacement current session.
#[test]
fn provider_capture_attribution_survives_session_rollover() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness
        .switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
        .expect("switch session");
    let provider = harness
        .extensions
        .entries
        .iter()
        .find_map(|(connection_id, entry)| {
            (entry.kind == tau_proto::ClientKind::Provider && entry.state == ExtensionState::Ready)
                .then(|| connection_id.clone())
        })
        .expect("ready provider");
    let capture = tau_proto::ProviderDebugCapture {
        session_id: test_session_id("s1"),
        agent_prompt_id: tau_proto::AgentPromptId::parse("late-prompt").expect("prompt"),
        class: tau_proto::ProviderDebugCaptureClass::HttpSseResponse,
        zstd: vec![1, 2, 3],
    };
    assert!(
        harness.sessions_dir().join("s1").is_dir(),
        "old durable session root remains"
    );

    let (target, instance) = harness
        .provider_debug_capture_target(&provider, &capture)
        .expect("late durable attribution");

    assert!(target.ends_with("sessions/s1"));
    assert_eq!(instance.as_str(), "provider");
}
/// Ensures a failed direct provider prompt route unwinds in-flight prompt
/// bookkeeping and emits user-visible lifecycle diagnostics.
#[test]
fn provider_prompt_route_failure_clears_prompt_bookkeeping() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let model: tau_proto::ModelId = "test/model".into();
    h.provider_runtime
        .model_routes
        .insert(model.clone(), crate::test_connection_id("missing-provider"));
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .model_override = Some(model);

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("route failure".to_owned()))
        .expect("dispatch checkpointed prompt");
    let agent_prompt_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptStarted(started) if started.model == "test/model".into() => {
                Some(started.agent_prompt_id)
            }
            _ => None,
        })
        .expect("compact prompt fact committed before route failure");

    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(agent_prompt_id.as_str())
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .models
            .contains_key(&agent_prompt_id)
    );
    assert!(
        !h.provider_runtime
            .pending_prompts
            .contains_key(&agent_prompt_id)
    );
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent still loaded");
    assert_eq!(conv.dispatch.in_flight_prompt, None);
    assert_eq!(conv.dispatch.last_prompt_id, None);
    assert!(matches!(conv.turn.turn_state, AgentTurnState::Idle));
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        0
    );

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(finished)
            if finished.agent_prompt_id == agent_prompt_id
                && finished.stop_reason == tau_proto::ProviderStopReason::Error
                && finished.output_items.is_empty()
    )));
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentPromptTerminated(terminated)
            if terminated.agent_prompt_id == agent_prompt_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::HarnessNotice(info)
            if info.message.contains("provider prompt route failed")
                && info.message.contains(agent_prompt_id.as_str())
    )));

    h.shutdown().expect("shutdown");
}

/// Losing the captured route after successor-owner commit must durably order
/// prompt-start before failure while never exposing the request to a provider.
#[test]
fn output_length_prompt_start_route_loss_terminalizes_before_provider_delivery() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "route-bound continuation".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let _interceptor = connect_test_tool(&mut h, "length-owner-interceptor");
    h.handle_extension_event(
        "length-owner-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register owner interceptor");
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 5))
        .expect("source length response");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "successor prompt-start is parked"
    );
    h.provider_runtime.model_routes.remove(&source.model);
    h.handle_extension_event(
        "length-owner-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release prompt-start after route loss");
    if h.runtime_io.publication.pending_intercept.is_some() {
        h.handle_extension_event(
            "length-owner-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release synthetic failure prompt-start");
    }

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let owner = records
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(owner)
                if owner.output_length_continuation.is_some() =>
            {
                Some(owner)
            }
            _ => None,
        })
        .expect("successor owner");
    let start_position = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == owner.agent_prompt_id
            )
        })
        .expect("synthetic prompt-start");
    let failure_position = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == owner.agent_prompt_id
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                                ..
                            }
                        )
            )
        })
        .expect("route failure terminal");
    assert!(start_position < failure_position);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1,
        "reserved successor never reaches provider delivery"
    );
    h.shutdown().expect("shutdown");
}

/// Proves normal startup returns a redacted typed error, removes the malformed
/// source, and launches no configured process when a raw value is not Unicode.
#[cfg(unix)]
#[test]
fn raw_secret_source_error_prevents_provider_start() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process::Command;

    if std::env::var_os("TAU_RAW_SECRET_STARTUP_TEST").is_some() {
        let tempdir = TempDir::new().expect("tempdir");
        // Any spawn attempt fails synchronously as ExtensionSpawn, so observing
        // SecretSource proves discovery stopped startup before that boundary.
        let missing_executable = format!("/tau-raw-secret-test-missing-{}", std::process::id());
        let config = Config {
            extensions: BTreeMap::from([(
                "raw-secret-observer".to_owned(),
                ExtensionConfig {
                    tool_prefix: None,
                    name: "raw-secret-observer".to_owned(),
                    command: missing_executable,
                    args: Vec::new(),
                    role: None,
                    component: None,
                    require: true,
                    startup_timeout: Duration::from_secs(1),
                    cwd: None,
                    config: serde_json::json!({}),
                    secrets: BTreeMap::new(),
                    tau_state_access: TauStateAccess::Legacy,
                    tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
                },
            )]),
            extension_startup_diagnostics: Vec::new(),
            harness_settings: HarnessSettings::built_in(),
        };
        let mut initial_client_error_stream = None;
        let error = match Harness::from_config_with_initial_client(
            &config,
            tempdir.path().join("state"),
            tau_config::settings::TauDirs {
                config_dir: None,
                state_dir: Some(tempdir.path().join("runtime")),
            },
            "raw-secret-source",
            HarnessSessionLaunch {
                mode: HarnessSessionLaunchMode::New,
                storage_mode: crate::HarnessStorageMode::Durable,
            },
            HarnessStartupInputs {
                initial_client: None,
                internal_tool_handlers: Vec::new(),
                ignore_startup_environment: false,
                memory_only_agent_store: false,
                project_root: tempdir.path().canonicalize().expect("project root"),
            },
            &mut initial_client_error_stream,
        ) {
            Ok(_) => panic!("raw secret source must fail startup"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                HarnessError::SecretSource(
                    tau_config::secret_sources::SecretSourceError::EnvironmentValueNotUnicode
                )
            ),
            "unexpected startup error: {error:?}"
        );
        assert!(!error.to_string().contains("startup-secret-value"));
        assert!(
            !std::env::vars_os()
                .any(|(key, _)| { tau_config::secret_sources::is_secret_environment_key(&key) }),
            "failed startup must consume malformed matching sources"
        );
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", RAW_SECRET_STARTUP_TEST, "--nocapture"])
        .env_clear()
        .env("TAU_RAW_SECRET_STARTUP_TEST", "1")
        .env(
            "TAU_SECRET_STARTUP",
            OsString::from_vec(b"startup-secret-value\xff".to_vec()),
        )
        .output()
        .expect("launch raw secret startup subprocess");
    assert!(
        output.status.success(),
        "raw secret startup subprocess failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Ensures malformed startup settings fail before even an injected provider
/// process starts, preventing built-in or configured fallback authority.
#[test]
fn malformed_settings_start_no_provider_process() {
    fn provider_must_not_start(_: UnixStream, _: UnixStream) -> Result<(), String> {
        INVALID_CONFIG_PROVIDER_STARTS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    INVALID_CONFIG_PROVIDER_STARTS.store(0, Ordering::SeqCst);
    let tempdir = TempDir::new().expect("tempdir");
    let config_dir = tempdir.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("harness.yaml"), "extensions: [malformed\n")
        .expect("write malformed config");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(tempdir.path().join("runtime")),
    };

    let result = Harness::new_with_provider(
        tempdir.path().join("state"),
        dirs,
        provider_must_not_start,
        Vec::new(),
        "invalid-config",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    );

    assert!(result.is_err(), "malformed settings must fail startup");
    assert_eq!(
        INVALID_CONFIG_PROVIDER_STARTS.load(Ordering::SeqCst),
        0,
        "startup must validate settings before launching a provider"
    );
}

/// Proves session-ephemeral and memory-only harnesses never turn Provider
/// attribution into a filesystem capture target.
#[test]
fn provider_capture_target_requires_durable_storage() {
    for (name, mode) in [
        ("ephemeral", crate::HarnessStorageMode::SessionEphemeral),
        ("memory", crate::HarnessStorageMode::MemoryOnly),
    ] {
        let temp = TempDir::new().expect("tempdir");
        let harness = quiet_provider_harness_with_start_reason_and_storage_mode(
            temp.path(),
            tau_proto::SessionStartReason::Initial,
            mode,
        )
        .expect("harness");
        let provider = harness
            .extensions
            .entries
            .iter()
            .find_map(|(connection_id, entry)| {
                (entry.kind == tau_proto::ClientKind::Provider
                    && entry.state == ExtensionState::Ready)
                    .then(|| connection_id.clone())
            })
            .expect("ready provider");
        let capture = tau_proto::ProviderDebugCapture {
            session_id: test_session_id("s1"),
            agent_prompt_id: tau_proto::AgentPromptId::parse("prompt").expect("prompt"),
            class: tau_proto::ProviderDebugCaptureClass::HttpSseRequest,
            zstd: vec![1],
        };
        assert!(
            harness
                .provider_debug_capture_target(&provider, &capture)
                .is_none(),
            "{name} storage must reject capture persistence"
        );
    }
}

/// Proves only a ready authenticated Provider with an existing attributed
/// durable-session root can select a capture target.
#[test]
fn provider_capture_target_rejects_unknown_or_unauthorized_attribution() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let provider = harness
        .extensions
        .entries
        .iter()
        .find_map(|(connection_id, entry)| {
            (entry.kind == tau_proto::ClientKind::Provider && entry.state == ExtensionState::Ready)
                .then(|| connection_id.clone())
        })
        .expect("ready provider");
    let capture = tau_proto::ProviderDebugCapture {
        session_id: test_session_id("s1"),
        agent_prompt_id: tau_proto::AgentPromptId::parse("prompt").expect("prompt"),
        class: tau_proto::ProviderDebugCaptureClass::HttpSseRequest,
        zstd: vec![1],
    };

    harness
        .extensions
        .entries
        .get_mut(&provider)
        .expect("entry")
        .kind = tau_proto::ClientKind::Tool;
    assert!(
        harness
            .provider_debug_capture_target(&provider, &capture)
            .is_none()
    );
    let entry = harness
        .extensions
        .entries
        .get_mut(&provider)
        .expect("entry");
    entry.kind = tau_proto::ClientKind::Provider;
    entry.state = ExtensionState::Handshaking;
    assert!(
        harness
            .provider_debug_capture_target(&provider, &capture)
            .is_none()
    );
    harness
        .extensions
        .entries
        .get_mut(&provider)
        .expect("entry")
        .state = ExtensionState::Ready;
    let unknown = tau_proto::ProviderDebugCapture {
        session_id: test_session_id("unknown"),
        ..capture
    };
    assert!(
        harness
            .provider_debug_capture_target(&provider, &unknown)
            .is_none()
    );
}

/// Proves a persistent Provider receives its exact settings snapshot so the
/// provider-only gate does not accidentally deny the authorized positive case.
#[test]
fn persistent_provider_configure_includes_settings_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let settings = tau_config::settings::extension_provider_settings_dir_of(&sp, "provider-work")
        .expect("settings path");
    std::fs::create_dir_all(&settings).expect("settings directory");
    std::fs::write(settings.join("provider.json"), b"providers").expect("settings");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.config.provider_settings_snapshots.insert(
        "provider-work".to_owned(),
        BTreeMap::from([("provider.json".to_owned(), b"providers".to_vec())]),
    );

    let configure =
        configure_supervised_extension(&mut h, "provider-work", tau_proto::ClientKind::Provider);

    assert_eq!(
        configure.settings_files.get("provider.json"),
        Some(&b"providers".to_vec())
    );
}

/// Proves a memory-only provider receives the preloaded credential-free
/// settings needed to advertise the models used by prompt and tool previews.
#[test]
fn memory_only_provider_configure_receives_settings_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let settings = tau_config::settings::extension_provider_settings_dir_of(&sp, "provider-memory")
        .expect("settings path");
    std::fs::create_dir_all(&settings).expect("stale settings directory");
    std::fs::write(settings.join("provider.json"), b"must-not-cross").expect("stale settings");
    let mut h = quiet_provider_harness_with_start_reason_and_storage_mode(
        &sp,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::MemoryOnly,
    )
    .expect("memory-only harness");
    h.config.provider_settings_snapshots.insert(
        "provider-memory".to_owned(),
        BTreeMap::from([("provider.json".to_owned(), b"preview-settings".to_vec())]),
    );

    let configure =
        configure_supervised_extension(&mut h, "provider-memory", tau_proto::ClientKind::Provider);

    assert_eq!(
        configure.settings_files["provider.json"],
        b"preview-settings".to_vec()
    );
}

/// Proves memory-only startup snapshots settings and suppresses credential
/// resolution and materialization.
#[test]
fn memory_only_provider_snapshot_omits_named_declaration_value() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    std::fs::create_dir_all(state.join("secrets")).expect("source root");
    std::fs::write(state.join("secrets/provider_key.yaml"), "must-not-cross").expect("source");
    let config =
        builtin_provider_startup_config(Some(tau_config::settings::ExtensionSecretEntry {
            optional: false,
        }));
    let snapshot = provider_startup::snapshot_memory_only_provider_settings(&config, None, &state)
        .expect("memory-only provider snapshot");
    let resolved = Harness::resolve_startup_extension_secrets(
        &config,
        &state,
        &SecretSources::default(),
        &snapshot.bound_names,
    )
    .expect("memory-only secret suppression");
    assert!(resolved.secrets["provider-work"].is_empty());
    let mut harness = quiet_provider_harness(state.join("harness")).expect("harness");

    let configure = configure_supervised_extension(
        &mut harness,
        "provider-work",
        tau_proto::ClientKind::Provider,
    );

    assert!(configure.settings_files.is_empty());
    assert!(configure.secrets.is_empty());
    assert!(
        !state
            .join(
                "secrets/ext/provider-work/providers/0123456789abcdef0123456789abcdef/api-key.json"
            )
            .exists()
    );
}

/// Proves startup materialization, source selection, and Configure all use one
/// settings generation retained after the instance lifecycle lock is released.
#[test]
fn provider_startup_retains_materialized_settings_snapshot() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let settings =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("settings");
    std::fs::create_dir_all(&settings).expect("settings root");
    let original = named_provider_settings();
    std::fs::write(settings.join("deepseek.json"), &original).expect("settings");
    std::fs::create_dir_all(state.join("secrets")).expect("source root");
    std::fs::write(state.join("secrets/provider_key.yaml"), " first-key \n").expect("source");

    let ProviderStartupSnapshot {
        settings: snapshots,
        bound_names,
        diagnostics,
        ..
    } = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &builtin_provider_startup_config(Some(tau_config::settings::ExtensionSecretEntry {
            optional: false,
        })),
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("startup snapshot");

    std::fs::write(
        settings.join("deepseek.json"),
        br#"{"changed":"after-snapshot"}"#,
    )
    .expect("concurrent replacement");
    let mut harness = quiet_provider_harness(state.join("configure-state")).expect("harness");
    harness.config.provider_settings_snapshots = snapshots;
    let configure = configure_supervised_extension(
        &mut harness,
        "provider-work",
        tau_proto::ClientKind::Provider,
    );
    assert_eq!(
        configure.settings_files["deepseek.json"], original,
        "Configure must use the generation that authorized materialization"
    );
    assert!(bound_names["provider-work"].contains("provider_key"));
    assert!(diagnostics.is_empty());
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join(
            "secrets/ext/provider-work/providers/0123456789abcdef0123456789abcdef/api-key.json",
        ))
        .expect("materialized record"),
    )
    .expect("typed record");
    assert_eq!(record["value"], "first-key");
}

/// Proves portable config symlinks and strict mutable state form one
/// deterministic disjoint startup snapshot.
#[cfg(unix)]
#[test]
fn provider_startup_merges_config_symlink_and_state_profiles() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let deployed = temp.path().join("nix-store-profile.json");
    std::fs::write(&deployed, br#"{"portable":true}"#).expect("portable profile");
    std::fs::set_permissions(&deployed, Permissions::from_mode(0o444))
        .expect("read-only deployment");
    let config = temp.path().join("config");
    let config_profiles = config.join("providers/provider-work");
    std::fs::create_dir_all(&config_profiles).expect("config root");
    symlink(&deployed, config_profiles.join("portable.json")).expect("leaf profile symlink");
    let state_profiles =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("state profiles");
    std::fs::create_dir_all(&state_profiles).expect("state profile root");
    std::fs::write(state_profiles.join("local.json"), br#"{"local":true}"#).expect("state profile");
    let mut startup = builtin_provider_startup_config(None);
    startup
        .extensions
        .get_mut("provider-work")
        .expect("provider")
        .component = None;

    let snapshot = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &startup,
        Some(&config),
        &state,
        &SecretSources::default(),
    )
    .expect("merged startup snapshot");

    assert_eq!(
        snapshot.settings["provider-work"]
            .keys()
            .collect::<Vec<_>>(),
        vec!["local.json", "portable.json"]
    );
}

/// Proves broken and non-regular config profile targets fail with source-aware
/// diagnostics instead of being skipped.
#[cfg(unix)]
#[test]
fn provider_startup_rejects_unusable_config_symlink_targets() {
    use std::os::unix::fs::symlink;

    for non_regular in [false, true] {
        let temp = TempDir::new().expect("tempdir");
        let state = temp.path().join("state");
        let config = temp.path().join("config");
        let profiles = config.join("providers/provider-work");
        std::fs::create_dir_all(&profiles).expect("profiles");
        let target = temp.path().join("target");
        if non_regular {
            std::fs::create_dir(&target).expect("directory target");
        }
        symlink(&target, profiles.join("bad.json")).expect("profile symlink");

        let error = provider_startup::snapshot_memory_only_provider_settings(
            &builtin_provider_startup_config(None),
            Some(&config),
            &state,
        )
        .expect_err("unusable target must fail");

        assert!(error.to_string().contains("config profile"));
        assert!(
            !state.exists(),
            "memory-only snapshot must not create state"
        );
    }
}

/// Proves memory-only discovery never waits on an externally contended config
/// inode because config is data, not lifecycle-lock authority.
#[cfg(unix)]
#[test]
fn memory_only_provider_snapshot_never_locks_external_config() {
    use fs2::FileExt as _;

    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config/providers/provider-work");
    std::fs::create_dir_all(&config).expect("config root");
    std::fs::write(config.join("portable.json"), b"{}").expect("profile");
    let external = File::open(&config).expect("config directory");
    external.lock_exclusive().expect("external lock");
    let config_root = temp.path().join("config");
    let state = temp.path().join("state");
    let startup = builtin_provider_startup_config(None);
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(provider_startup::snapshot_memory_only_provider_settings(
            &startup,
            Some(&config_root),
            &state,
        ))
        .expect("result");
    });

    let result = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("config lock must not block discovery");
    fs2::FileExt::unlock(&external).expect("unlock");
    worker.join().expect("worker");
    assert!(result.is_ok());
}

/// Proves config/state path aliasing is a logical duplicate error and never a
/// second lock acquisition.
#[cfg(unix)]
#[test]
fn provider_startup_rejects_config_state_alias_without_deadlock() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let state_profiles = state.join("providers/provider-work");
    std::fs::create_dir_all(&state_profiles).expect("state profiles");
    std::fs::write(state_profiles.join("alias.json"), b"{}").expect("profile");
    let config = temp.path().join("config");
    std::fs::create_dir_all(config.join("providers")).expect("config providers");
    symlink(&state_profiles, config.join("providers/provider-work")).expect("logical alias");

    let error = provider_startup::snapshot_memory_only_provider_settings(
        &builtin_provider_startup_config(None),
        Some(&config),
        &state,
    )
    .expect_err("alias duplicate");

    assert!(
        error
            .to_string()
            .contains("duplicated across config and state")
    );
}

/// Proves identical cross-layer bytes remain an ownership error rather than
/// being silently coalesced.
#[test]
fn provider_startup_rejects_cross_layer_duplicate_profiles() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let config = temp.path().join("config");
    for root in [&state, &config] {
        let profiles = root.join("providers/provider-work");
        std::fs::create_dir_all(&profiles).expect("profile root");
        std::fs::write(profiles.join("duplicate.json"), b"{}").expect("profile");
    }

    let error = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &builtin_provider_startup_config(None),
        Some(&config),
        &state,
        &SecretSources::default(),
    )
    .expect_err("duplicate must fail");

    assert!(
        error
            .to_string()
            .contains("duplicated across config and state")
    );
}

/// Proves optional duplicate failures retain safe source identity in the
/// visible startup diagnostic.
#[test]
fn optional_provider_duplicate_diagnostic_is_source_aware() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let config = temp.path().join("config");
    for root in [&state, &config] {
        let profiles = root.join("providers/provider-work");
        std::fs::create_dir_all(&profiles).expect("profile root");
        std::fs::write(profiles.join("duplicate.json"), b"{}").expect("profile");
    }
    let mut startup = builtin_provider_startup_config(None);
    startup
        .extensions
        .get_mut("provider-work")
        .expect("provider")
        .require = false;

    let snapshot =
        provider_startup::snapshot_memory_only_provider_settings(&startup, Some(&config), &state)
            .expect("optional snapshot");

    assert!(snapshot.skipped_extensions.contains("provider-work"));
    assert!(
        snapshot.diagnostics[0]
            .message
            .contains("duplicated across config and state")
    );
}

/// Proves the retired state location is wholly undiscovered.
#[test]
fn provider_startup_ignores_retired_provider_settings_tree() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let retired = state.join("provider-settings/provider-work");
    std::fs::create_dir_all(&retired).expect("retired root");
    std::fs::write(retired.join("legacy.json"), b"not even json").expect("retired profile");

    let snapshot = provider_startup::snapshot_memory_only_provider_settings(
        &builtin_provider_startup_config(None),
        None,
        &state,
    )
    .expect("snapshot");

    assert!(snapshot.settings["provider-work"].is_empty());
}

/// Proves the 4,096-file limit applies to the merged generation rather than
/// independently permitting a full limit in each source.
#[test]
fn provider_startup_applies_file_bound_across_merged_sources() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state/providers/provider-work");
    let config = temp.path().join("config/providers/provider-work");
    std::fs::create_dir_all(&state).expect("state root");
    std::fs::create_dir_all(&config).expect("config root");
    for index in 0..2_048 {
        std::fs::write(config.join(format!("c{index:04}.json")), b"{}").expect("config profile");
    }
    for index in 0..2_049 {
        std::fs::write(state.join(format!("s{index:04}.json")), b"{}").expect("state profile");
    }

    let error = provider_startup::snapshot_memory_only_provider_settings(
        &builtin_provider_startup_config(None),
        Some(&temp.path().join("config")),
        temp.path().join("state").as_path(),
    )
    .expect_err("merged file bound");

    assert!(error.to_string().contains("exceeds 4096 files"));
}

/// Proves aggregate bytes from config and state share one protocol-safe budget.
#[test]
fn provider_startup_applies_byte_bound_across_merged_sources() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state/providers/provider-work");
    let config = temp.path().join("config/providers/provider-work");
    std::fs::create_dir_all(&state).expect("state root");
    std::fs::create_dir_all(&config).expect("config root");
    let one_mib = vec![b' '; 1024 * 1024];
    for index in 0..8 {
        std::fs::write(config.join(format!("c{index}.json")), &one_mib).expect("config profile");
        std::fs::write(state.join(format!("s{index}.json")), &one_mib).expect("state profile");
    }

    let error = provider_startup::snapshot_memory_only_provider_settings(
        &builtin_provider_startup_config(None),
        Some(&temp.path().join("config")),
        temp.path().join("state").as_path(),
    )
    .expect_err("merged byte bound");

    assert!(
        error
            .to_string()
            .contains("merged provider snapshot exceeds")
    );
}

/// Proves a missing declaration overwrites an older named materialization with
/// an empty typed record and emits only a value-redacted disabling diagnostic.
#[test]
fn provider_startup_missing_declaration_suppresses_stale_credential() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let settings =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("settings");
    std::fs::create_dir_all(&settings).expect("settings root");
    std::fs::write(settings.join("deepseek.json"), named_provider_settings()).expect("settings");
    let credential = state
        .join("secrets/ext/provider-work/providers/0123456789abcdef0123456789abcdef/api-key.json");
    std::fs::create_dir_all(credential.parent().expect("parent")).expect("credential root");
    std::fs::write(
        &credential,
        br#"{"version":0,"kind":"api_key","value":"stale-secret"}"#,
    )
    .expect("stale credential");

    let ProviderStartupSnapshot {
        bound_names,
        diagnostics,
        ..
    } = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &builtin_provider_startup_config(None),
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("startup snapshot");

    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&credential).expect("credential"))
            .expect("typed record");
    assert_eq!(record["value"], "");
    assert!(bound_names["provider-work"].contains("provider_key"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("deepseek"));
    assert!(!diagnostics[0].message.contains("stale-secret"));
    let mut harness = quiet_provider_harness(state.join("notice-state")).expect("harness");
    harness.emit_extension_startup_diagnostics(&diagnostics);
    assert!(event_log_contains_source_event(
        &harness,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.purpose == tau_proto::NoticePurpose::Alert
                    && notice.level == tau_proto::NoticeLevel::Warning
                    && notice.message.contains("deepseek")
                    && !notice.message.contains("stale-secret")
        )
    ));
}

/// Proves provider snapshot failures follow the extension's required/optional
/// startup policy instead of unconditionally aborting the harness.
#[cfg(unix)]
#[test]
fn provider_startup_settings_failure_skips_optional_but_rejects_required() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    symlink(temp.path(), state.join("providers")).expect("settings symlink");
    let mut optional = builtin_provider_startup_config(None);
    optional
        .extensions
        .get_mut("provider-work")
        .expect("provider")
        .require = false;

    let snapshot = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &optional,
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("optional provider failure");
    assert!(snapshot.skipped_extensions.contains("provider-work"));
    assert_eq!(snapshot.diagnostics.len(), 1);
    assert!(
        provider_startup::snapshot_and_materialize_named_provider_credentials(
            &builtin_provider_startup_config(None),
            None,
            &state,
            &SecretSources::default(),
        )
        .is_err()
    );
}

/// Proves malformed source data never destructively replaces the previous
/// credential; optional providers skip while required providers fail startup.
#[test]
fn provider_startup_source_error_preserves_stale_credential() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let settings =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("settings");
    std::fs::create_dir_all(&settings).expect("settings root");
    std::fs::write(settings.join("deepseek.json"), named_provider_settings()).expect("settings");
    std::fs::create_dir_all(state.join("secrets")).expect("source root");
    std::fs::write(state.join("secrets/provider_key.yaml"), [0xff]).expect("invalid source");
    let credential = state
        .join("secrets/ext/provider-work/providers/0123456789abcdef0123456789abcdef/api-key.json");
    std::fs::create_dir_all(credential.parent().expect("credential parent"))
        .expect("credential root");
    let stale = br#"{"version":0,"kind":"api_key","value":"stale"}"#;
    std::fs::write(&credential, stale).expect("stale credential");
    let mut optional =
        builtin_provider_startup_config(Some(tau_config::settings::ExtensionSecretEntry {
            optional: false,
        }));
    optional
        .extensions
        .get_mut("provider-work")
        .expect("provider")
        .require = false;

    let snapshot = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &optional,
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("optional provider failure");
    assert!(snapshot.skipped_extensions.contains("provider-work"));
    assert_eq!(std::fs::read(&credential).expect("credential"), stale);
    let error = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &builtin_provider_startup_config(Some(tau_config::settings::ExtensionSecretEntry {
            optional: false,
        })),
        None,
        &state,
        &SecretSources::default(),
    )
    .expect_err("required source error")
    .to_string();
    assert!(!error.contains(state.to_string_lossy().as_ref()));
    assert!(!error.contains("provider_key.yaml"));
    assert_eq!(std::fs::read(credential).expect("credential"), stale);
}

/// Proves direct stored credentials remain outside rematerialization authority
/// while an explicit keyless marker creates and changes no Secret state.
#[test]
fn provider_startup_preserves_direct_and_explicit_keyless_secret_state() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let settings =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("settings");
    std::fs::create_dir_all(&settings).expect("settings root");
    std::fs::write(
        settings.join("direct.json"),
        serde_json::to_vec(&serde_json::json!({
            "kind": "chat_completions",
            "credential": {
                "kind": "api_key",
                "identity": "fedcba9876543210fedcba9876543210"
            }
        }))
        .expect("direct settings"),
    )
    .expect("direct settings file");
    std::fs::write(
        settings.join("keyless.json"),
        serde_json::to_vec(&serde_json::json!({
            "kind": "chat_completions",
            "credential": {"kind": "none"}
        }))
        .expect("keyless settings"),
    )
    .expect("keyless settings file");
    let direct_credential = state
        .join("secrets/ext/provider-work/providers/fedcba9876543210fedcba9876543210/api-key.json");
    std::fs::create_dir_all(direct_credential.parent().expect("parent")).expect("credential root");
    let direct_record = serde_json::to_vec(&serde_json::json!({
        "version": 0, "kind": "api_key", "value": "direct-key"
    }))
    .expect("direct record");
    std::fs::write(&direct_credential, &direct_record).expect("direct credential");
    let keyless_credential = state.join("secrets/ext/provider-work/providers/keyless/api-key.json");

    let snapshot = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &builtin_provider_startup_config(None),
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("startup snapshot");

    assert!(snapshot.settings["provider-work"].contains_key("keyless.json"));
    assert_eq!(
        std::fs::read(direct_credential).expect("direct credential"),
        direct_record
    );
    assert!(!keyless_credential.exists());
}

/// Proves all configured Provider instances retain bounded settings snapshots,
/// while named credential materialization remains restricted to the exact
/// built-in provider component.
#[test]
fn custom_provider_retains_settings_without_builtin_materialization() {
    let temp = TempDir::new().expect("tempdir");
    let state = temp.path().join("state");
    let settings =
        tau_config::settings::extension_provider_settings_dir_of(&state, "provider-work")
            .expect("settings");
    std::fs::create_dir_all(&settings).expect("settings root");
    std::fs::write(settings.join("custom.json"), br#"{"custom":true}"#).expect("settings");
    let mut config = builtin_provider_startup_config(None);
    config
        .extensions
        .get_mut("provider-work")
        .expect("provider")
        .args = vec!["custom-provider".to_owned()];

    let ProviderStartupSnapshot {
        settings: snapshots,
        bound_names,
        diagnostics,
        ..
    } = provider_startup::snapshot_and_materialize_named_provider_credentials(
        &config,
        None,
        &state,
        &SecretSources::default(),
    )
    .expect("startup snapshot");

    assert_eq!(
        snapshots["provider-work"]["custom.json"],
        br#"{"custom":true}"#
    );
    assert!(bound_names.is_empty());
    assert!(diagnostics.is_empty());
    assert!(!state.join("secrets/ext/provider-work").exists());
}

/// Concrete provider model metadata, rather than a manually assembled template
/// context, controls parallel-tool guidance in normal and preview rendering.
#[test]
fn provider_model_parallel_capability_flows_into_prompt_rendering() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let model: tau_proto::ModelId = "test/model".parse().expect("model id");
    let mut info = staged_provider_model("test/model");
    info.supported_tool_types.clear();
    info.supports_parallel_tool_calls = true;
    h.provider_runtime.model_info.insert(model.clone(), info);
    h.config.selected_model = Some(model);

    let normal = h.build_system_prompt_for_role(&h.config.selected_role);
    let preview_agent_id = crate::parse_agent_id("preview-context");
    let preview = h
        .build_system_prompt_for_role_preview(&h.config.selected_role, &preview_agent_id)
        .expect("preview prompt");

    assert!(normal.contains("at most one tool call"));
    assert!(preview.contains("at most one tool call"));
    assert!(!normal.contains("multiple independent tool calls"));
    assert!(!preview.contains("multiple independent tool calls"));
    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_provider_is_rejected_without_ambiguous_fallback() {
    // A different connection cannot become a hidden fallback owner selected by
    // registration arrival order.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let report = h
        .tool_routing
        .registry
        .register(&crate::test_connection_id("conn-duplicate-shell"), spec);
    assert!(!report.errors.is_empty());
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));

    unregister_shell(&mut h);
    assert!(h.tool_routing.registry.providers_for("shell").is_empty());

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("after partial unregister".to_owned()),
    )
    .expect("dispatch user prompt");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        context_text_count(&prompt, &crate::internal_envelope::frame(&notice)),
        1
    );
    assert_eq!(agent_prompt_text_count(&h, &notice), 1);
    assert!(!prompt_has_tool(&prompt, "shell"));

    h.shutdown().expect("shutdown");
}
