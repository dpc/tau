use super::*;

const COMPAT_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/compat");

fn read_compat_fixture(relative: &str) -> String {
    std::fs::read_to_string(format!("{COMPAT_FIXTURE_ROOT}/{relative}"))
        .unwrap_or_else(|error| panic!("read compatibility fixture {relative}: {error}"))
}

fn load_profile_fixture(relative: &str) -> BuiltinProviderProfile {
    serde_json::from_str(&read_compat_fixture(relative))
        .unwrap_or_else(|error| panic!("decode compatibility profile {relative}: {error}"))
}

fn assert_canonical_profile_fixture(relative: &str) -> BuiltinProviderProfile {
    let source = read_compat_fixture(relative);
    let profile: BuiltinProviderProfile = serde_json::from_str(&source)
        .unwrap_or_else(|error| panic!("decode compatibility profile {relative}: {error}"));
    let encoded = format!(
        "{}\n",
        serde_json::to_string_pretty(&profile).expect("serialize compatibility profile")
    );
    assert_eq!(encoded, source, "canonical profile changed: {relative}");
    profile
}

/// Old auth profile files for every persisted provider kind must continue to
/// decode and canonically reserialize byte-for-byte.
#[test]
fn old_profile_fixtures_round_trip_canonically() {
    for fixture in [
        "profiles/chatgpt.json",
        "profiles/chat_completions.json",
        "profiles/openrouter.json",
    ] {
        assert_canonical_profile_fixture(fixture);
    }
}

fn compatibility_profiles() -> BuiltinProviderProfiles {
    BuiltinProviderProfiles {
        providers: BTreeMap::from([
            (
                ProviderName::new("chatgpt"),
                load_profile_fixture("profiles/chatgpt.json"),
            ),
            (
                ProviderName::new("local"),
                load_profile_fixture("profiles/chat_completions.json"),
            ),
            (
                ProviderName::new("router"),
                load_profile_fixture("profiles/openrouter.json"),
            ),
        ]),
    }
}

fn compatibility_route_snapshot(
    model: ModelId,
    profiles: &mut BuiltinProviderProfiles,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
) -> serde_json::Value {
    let requested = model.to_string();
    match resolve_prompt_backend(&model, profiles, refresh_rejections, &test_network_policy()) {
        Some(PromptBackend::Responses(config)) => serde_json::json!({
            "requested": requested,
            "backend": "responses",
            "model": config.model_id(),
            "mode": match config.mode() {
                CodexMode::Standard => "standard",
                CodexMode::LiteCompatibility => "lite_compatibility",
            },
            "base_url": config.base_url(),
            "surface": "chatgpt",
            "credential_present": config.has_credential(),
            "raw_context_window": config.raw_context_window(),
            "account_id_present": config.has_account_id(),
            "supports_reasoning_effort": config.supports_reasoning_effort(),
            "supports_reasoning_summary": config.supports_reasoning_summary(),
            "supports_verbosity": config.supports_verbosity(),
            "supports_phase": config.supports_phase(),
            "supports_encrypted_reasoning": config.supports_encrypted_reasoning(),
            "supports_websocket": true,
            "supports_compaction": config.supports_compaction(),
            "supports_prompt_cache_key": config.supports_prompt_cache_key(),
        }),
        Some(PromptBackend::ChatCompletions { provider, model }) => serde_json::json!({
            "requested": requested,
            "backend": "chat_completions",
            "provider": {
                "base_url": provider.base_url,
                "credential_present": !provider.api_key.trim().is_empty(),
                "max_output_tokens": provider.max_output_tokens,
                "extra_body": provider.extra_body,
                "compat": provider.compat,
            },
            "model": model,
        }),
        Some(PromptBackend::Unavailable) => serde_json::json!({
            "requested": requested,
            "backend": "unavailable",
        }),
        None => serde_json::json!({
            "requested": requested,
            "backend": null,
        }),
    }
}

fn responses_event_snapshot() -> Vec<Event> {
    let mut prompt = minimal_prompt();
    prompt.agent_prompt_id = "compat-prompt".into();
    prompt.agent_id = tau_proto::AgentId::parse("compat-agent").expect("agent id");
    prompt.session_id = "compat-session".into();
    let request = CodexPrompt {
        system_prompt: "",
        context: &prompt.context,
        tools: &prompt.tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        compaction: prompt.compaction,
        originator: &prompt.originator,
        session_id: &prompt.session_id,
        agent_id: &prompt.agent_id,
        share_user_cache_key: prompt.share_user_cache_key,
        debug_provider_requests: false,
    };
    let mut state = tau_provider_codex::test_stream_state();
    tau_provider_codex::test_append_message_delta(&mut state, 0, "compatibility text");
    let state = state.with_terminal_facts(10, 4, 2, "resp-compat".to_owned());
    let backend = ProviderBackend {
        kind: ProviderBackendKind::Responses,
        base_url: tau_provider_codex::DEFAULT_BASE_URL.to_owned(),
        transport: ProviderBackendTransport::Websocket,
        stale_chain_fallback: true,
    };
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let emitted = emit_chatgpt_stream_update(
            prompt.agent_prompt_id.as_str(),
            &prompt.agent_id,
            &prompt.originator,
            &state,
            &mut CodexStreamDeltaEmitter::default(),
            ProviderResponseStats {
                previous: tau_proto::ProviderResponseStatsSample::default(),
                current: tau_proto::ProviderResponseStatsSample {
                    response_bytes_received: 42,
                    elapsed_micros: 1_500,
                },
            },
            &mut writer,
        );
        assert!(emitted, "compatibility update must cross the output seam");
        finish_stream(
            prompt.session_id.as_str(),
            prompt.agent_prompt_id.as_str(),
            &prompt,
            &request,
            &backend,
            state,
            tau_provider_codex::test_debug_capture(),
            Some(tau_proto::WsPoolDelta {
                upgrades: 1,
                silent_reconnects: 0,
            }),
            false,
            &mut writer,
        )
        .expect("finish compatibility response");
    }
    decode_frames(&bytes)
        .into_iter()
        .filter_map(|message| match message {
            tau_proto::HarnessInputMessage::Emit(emit) => Some(*emit.event),
            _ => None,
        })
        .collect()
}

fn chat_completions_event_snapshot(
    mut provider: ChatCompletionsProvider,
    model: ChatCompletionsModel,
    provider_name: &str,
) -> Vec<Event> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind compatibility server");
    let address = listener.local_addr().expect("compatibility server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept compatibility request");
        let mut request = [0_u8; 8192];
        let _ = std::io::Read::read(&mut socket, &mut request).expect("read compatibility request");
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"compatibility text\"},",
            "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,",
            "\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        std::io::Write::write_all(&mut socket, response.as_bytes())
            .expect("write compatibility response");
    });
    provider.base_url = format!("http://{address}");
    let mut prompt = minimal_prompt();
    prompt.agent_prompt_id = format!("{provider_name}-compat-prompt").into();
    prompt.agent_id = tau_proto::AgentId::parse("compat-agent").expect("agent id");
    prompt.session_id = "compat-session".into();
    prompt.model = ModelId::new(ProviderName::new(provider_name), model.id.clone());
    let backend = PromptBackend::ChatCompletions { provider, model };
    let mut bytes = Vec::new();
    let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
    let mut abort = RecordingRetrySleeper;
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let retry = handle_prompt_backend(
            &prompt.agent_prompt_id,
            &backend,
            &prompt,
            &mut writer,
            &mut abort,
            ChatGptPromptExecutionContext {
                debug_provider_requests: false,
                runtime: &runtime,
            },
            &mut |_| {},
        )
        .expect("run compatibility Chat Completions prompt");
        assert!(retry.is_none(), "compatibility success must be terminal");
    }
    server.join().expect("join compatibility server");
    decode_frames(&bytes)
        .into_iter()
        .filter_map(|message| match message {
            tau_proto::HarnessInputMessage::Emit(mut emit) => {
                if let Event::ProviderResponseUpdatedReported(update) = emit.event.as_mut()
                    && let Some(stats) = update.response_stats.as_mut()
                {
                    stats.previous.elapsed_micros = 0;
                    stats.current.elapsed_micros = 0;
                }
                if let Event::ProviderResponseFinishedReported(finished) = emit.event.as_mut()
                    && let Some(backend) = finished.backend.as_mut()
                {
                    backend.base_url = "<loopback-fixture>".to_owned();
                }
                Some(*emit.event)
            }
            _ => None,
        })
        .collect()
}

fn compatibility_event_snapshot() -> serde_json::Value {
    let profiles = compatibility_profiles();
    let BuiltinProviderProfile::ChatCompletions(local) = profiles.providers["local"].clone() else {
        panic!("local compatibility profile kind")
    };
    let local_model = local.models[0].clone();
    let BuiltinProviderProfile::OpenRouter(router) = profiles.providers["router"].clone() else {
        panic!("router compatibility profile kind")
    };
    let router_model = router.models[0].clone();
    serde_json::json!({
        "responses": responses_event_snapshot(),
        "chat_completions": chat_completions_event_snapshot(local, local_model, "local"),
        "openrouter": chat_completions_event_snapshot(
            router.to_chat_completions(),
            router_model,
            "router",
        ),
    })
}

/// Model publication, exact configured-model routing, and public provider event
/// serialization form one compatibility snapshot for later ownership cutovers.
#[test]
fn model_routing_and_event_compatibility_snapshot() {
    let mut profiles = compatibility_profiles();
    let models = models_for_profiles(&profiles);
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();
    let routes = [
        ModelId::new(ProviderName::new("chatgpt"), ModelName::new("gpt-5.6-sol")),
        ModelId::new(ProviderName::new("local"), ModelName::new("local-model")),
        ModelId::new(ProviderName::new("router"), ModelName::new("vendor/model")),
        ModelId::new(ProviderName::new("local"), ModelName::new("missing")),
    ]
    .into_iter()
    .map(|model| compatibility_route_snapshot(model, &mut profiles, &mut refresh_rejections))
    .collect::<Vec<_>>();
    let actual = serde_json::json!({
        "models": models,
        "routes": routes,
        "events": compatibility_event_snapshot(),
    });
    let expected: serde_json::Value =
        serde_json::from_str(&read_compat_fixture("snapshots/models-routing-events.json"))
            .expect("decode compatibility snapshot");
    assert_eq!(
        actual,
        expected,
        "provider compatibility snapshot changed:\n{}",
        serde_json::to_string_pretty(&actual).expect("render actual snapshot")
    );
}

/// Durable provider-finished records from before transport metadata and from a
/// stale-chain WebSocket turn must retain their defaults and replay sidecars.
#[test]
fn legacy_provider_session_fixtures_decode() {
    let load = |stem: &str| {
        let expected: Event =
            serde_json::from_str(&read_compat_fixture(&format!("sessions/{stem}.json")))
                .expect("decode readable event fixture");
        let temporary = tempfile::tempdir().expect("temporary agent store");
        let agent_dir = temporary.path().join("legacy-agent");
        std::fs::create_dir(&agent_dir).expect("create fixture agent directory");
        std::fs::copy(
            format!("{COMPAT_FIXTURE_ROOT}/sessions/{stem}.events.cbor"),
            agent_dir.join("events.cbor"),
        )
        .expect("copy durable compatibility fixture");
        let store = tau_core::AgentStore::open(temporary.path()).expect("open legacy agent store");
        let records = store
            .agent_events("legacy-agent")
            .expect("replay legacy agent events");
        assert_eq!(records.len(), 1, "fixture must contain one durable event");
        assert_eq!(
            records[0].recorded_at,
            tau_proto::UnixMicros::default(),
            "pre-recorded_at journal must restore the historical default"
        );
        assert_eq!(records[0].event, expected);
        records[0].event.clone()
    };
    let Event::ProviderResponseFinished(pre_transport) = load("legacy-responses-pre-transport")
    else {
        panic!("pre-transport fixture must be a provider-finished event")
    };
    let backend = pre_transport.backend.expect("legacy backend");
    assert_eq!(backend.kind, ProviderBackendKind::Responses);
    assert_eq!(backend.transport, ProviderBackendTransport::HttpSse);
    assert!(!backend.stale_chain_fallback);
    assert_eq!(pre_transport.originator, tau_proto::PromptOriginator::User);

    let Event::ProviderResponseFinished(stale) = load("legacy-websocket-stale-chain") else {
        panic!("stale-chain fixture must be a provider-finished event")
    };
    let backend = stale.backend.expect("stale-chain backend");
    assert_eq!(backend.transport, ProviderBackendTransport::Websocket);
    assert!(backend.stale_chain_fallback);
    assert_eq!(
        stale.ws_pool_delta,
        Some(tau_proto::WsPoolDelta {
            upgrades: 1,
            silent_reconnects: 1,
        })
    );
    let ContextItem::Message(message) = &stale.output_items[0] else {
        panic!("legacy output message")
    };
    assert!(message.responses_raw_json.is_some());
}
