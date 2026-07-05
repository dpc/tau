use tau_provider_chat_completions::openrouter::OpenRouterProfile;

use super::*;

struct RecordingRetrySleeper {
    delays: Vec<std::time::Duration>,
}

impl RetrySleeper for RecordingRetrySleeper {
    fn sleep_or_abort(&mut self, delay: std::time::Duration, _current_apid: &str) -> SleepOutcome {
        self.delays.push(delay);
        SleepOutcome::Aborted
    }
}

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

#[derive(Default)]
struct TransportCounts {
    http_post_requests: std::sync::atomic::AtomicUsize,
}

fn spawn_ws_426_server() -> (String, Arc<TransportCounts>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    listener
        .set_nonblocking(true)
        .expect("set fake provider nonblocking");
    let addr = listener.local_addr().expect("fake provider addr");
    let counts = Arc::new(TransportCounts::default());
    let thread_counts = Arc::clone(&counts);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let mut request = [0_u8; 1024];
            let read = std::io::Read::read(&mut stream, &mut request).unwrap_or(0);
            if request[..read].starts_with(b"POST ") {
                thread_counts
                    .http_post_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let response = concat!(
                "HTTP/1.1 426 Upgrade Required\r\n",
                "Content-Length: 21\r\n",
                "Connection: close\r\n",
                "\r\n",
                "upgrade unavailable\n"
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
    (format!("http://{addr}/backend-api"), counts)
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
        "sp-error".into(),
        tau_proto::AgentId::parse("agent").expect("valid test agent id"),
        tau_proto::PromptOriginator::User,
        "no model specified",
    );

    assert!(finished.output_items.is_empty());
    assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(finished.error.as_deref(), Some("no model specified"));
}

#[test]
fn chatgpt_websocket_terminal_error_reports_websocket_backend() {
    // Regression for tau-agent-y8vc: when a WebSocket-capable ChatGPT/Codex
    // prompt fails before a stream is established, the final provider error
    // metadata should describe the attempted WebSocket transport and must not
    // be produced by an HTTP/SSE fallback POST.
    let (base_url, counts) = spawn_ws_426_server();
    let config = tau_provider_chatgpt::responses::ResponsesConfig {
        surface: tau_provider_chatgpt::responses::ResponsesSurface::ChatGpt,
        base_url: base_url.clone(),
        api_key: "token".to_owned(),
        model_id: "gpt-5.3-codex".to_owned(),
        context_window: 258_400,
        account_id: Some("account".to_owned()),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_verbosity: false,
        supports_phase: true,
        supports_encrypted_reasoning: false,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: false,
    };
    let mut prompt = minimal_prompt();
    prompt.model = "chatgpt/gpt-5.3-codex".parse().expect("model id");
    let mut retry = RecordingRetrySleeper { delays: Vec::new() };
    let runtime = ChatGptRuntime::new();
    let mut bytes = Vec::new();
    let mut writer = PeerOutputWriter::new(&mut bytes);

    handle_prompt(
        "ap-ws-terminal",
        &config,
        &prompt,
        false,
        &mut writer,
        &mut retry,
        &runtime,
    )
    .expect("prompt handler should emit terminal provider error");

    assert_eq!(
        counts
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "terminal WS error must not be produced by HTTP/SSE fallback"
    );
    let frames = decode_frames(&bytes);
    let finished = frames.iter().find_map(|frame| match frame {
        tau_proto::HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
            tau_proto::Event::ProviderResponseFinished(finished) => Some(finished),
            _ => None,
        },
        _ => None,
    });
    let finished = finished.expect("provider response finished frame");
    assert_eq!(
        finished.backend.as_ref().map(|backend| backend.transport),
        Some(tau_proto::ProviderBackendTransport::Websocket)
    );
    assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::Error);
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

#[test]
fn add_rejects_positional_arguments() {
    // `tau provider add` owns the full setup flow and prompts for both kind and
    // provider namespace, so stale direct forms must not keep working.
    let args = vec!["add".to_owned(), "chatgpt".to_owned()];

    let error = run_provider_cli(&args).expect_err("add arguments should fail");

    assert!(error.to_string().contains("does not accept arguments"));
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

    assert_eq!(chatgpt["kind"], "chatgpt");
    assert_eq!(chat_completions["kind"], "chat_completions");
    assert_eq!(openrouter["kind"], "openrouter");
}

#[test]
fn chat_completions_add_defaults_to_legacy_max_tokens() {
    // The setup wizard is usually used for local OpenAI-compatible servers.
    // Those should get Tau's output cap through `max_tokens`, not OpenAI's
    // newer `max_completion_tokens` spelling.
    let compat = chat_completions_add_compat();

    assert!(!compat.max_completion_tokens);
    assert!(compat.stream_options);
    assert!(compat.prompt_cache_key);
}

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
    }
}

#[test]
fn chat_completions_profiles_publish_and_route_only_configured_models() {
    // Chat Completions provider profiles are user-configured namespaces. The
    // provider must publish exactly the configured models and reject unknown
    // model ids instead of falling back to any ChatGPT/Codex backend.
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
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::ChatCompletions(provider),
        )]),
    };

    let models = models_for_profiles(&profiles);
    assert_eq!(model_ids(&models), vec!["local/llama"]);
    assert!(matches!(
        resolve_prompt_backend(
            &ModelId::new(provider_name.clone(), configured.id.clone()),
            &mut profiles
        ),
        Some(PromptBackend::ChatCompletions { model, .. }) if model.id == configured.id
    ));
    assert!(
        resolve_prompt_backend(
            &ModelId::new(provider_name, ModelName::new("missing")),
            &mut profiles,
        )
        .is_none()
    );
}

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
        providers: BTreeMap::from([(
            provider_name.clone(),
            BuiltinProviderProfile::OpenRouter(profile),
        )]),
    };

    let models = models_for_profiles(&profiles);
    assert_eq!(model_ids(&models), vec!["openrouter/anthropic/claude-test"]);
    assert!(matches!(
        resolve_prompt_backend(
            &ModelId::new(provider_name.clone(), configured.id.clone()),
            &mut profiles
        ),
        Some(PromptBackend::ChatCompletions { provider, model })
            if provider.base_url == "https://openrouter.ai/api/v1"
                && model.id == configured.id
    ));
    assert!(
        resolve_prompt_backend(
            &ModelId::new(provider_name, ModelName::new("missing")),
            &mut profiles,
        )
        .is_none()
    );
}

#[test]
fn provider_retry_after_delay_is_clamped_before_sleeping() {
    // Upstream account-limit responses can advertise reset windows measured in
    // hours. Prompt workers must clamp such delays before entering the sleeper
    // so one provider response cannot monopolize a worker indefinitely.
    let mut bytes = Vec::new();
    let mut writer = PeerOutputWriter::new(&mut bytes);
    let mut sleeper = RecordingRetrySleeper { delays: Vec::new() };
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "resets_in_seconds": u64::MAX,
        },
    })
    .to_string();

    let error = with_llm_retry(
        "sp-huge-retry",
        &tau_proto::AgentId::parse("agent").expect("agent id"),
        &tau_proto::PromptOriginator::User,
        &mut writer,
        &mut sleeper,
        |_writer, _sleeper| -> Result<(), common::LlmError> {
            Err(common::LlmError::HttpStatus(429, body.clone()))
        },
    )
    .expect_err("aborted retry should return original provider error");

    assert!(matches!(error, common::LlmError::HttpStatus(429, _)));
    assert_eq!(sleeper.delays, vec![LLM_MAX_RETRY_DELAY]);
}

#[test]
fn cancellation_sleep_aborts_when_deadline_would_overflow() {
    // `CancellationState` is the last line of defense for retry sleeping. Even
    // if a future caller forgets to clamp, impossible deadlines should abort
    // rather than panic on `Instant` arithmetic.
    let cancellation = CancellationState::default();

    assert_eq!(
        cancellation.sleep_or_abort(std::time::Duration::MAX, "sp-overflow"),
        SleepOutcome::Aborted
    );
}

#[test]
fn cancellation_waker_fires_for_matching_prompt_only() {
    // WebSocket turns park on provider events for up to the turn timeout. The
    // cancellation registry must therefore wake the matching turn directly,
    // without relying on periodic receive timeouts.
    let cancellation = Arc::new(CancellationState::default());
    let target_apid = tau_proto::AgentPromptId::from("ap-target");
    let other_apid = tau_proto::AgentPromptId::from("ap-other");
    let matching = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let other = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let _matching_guard = cancellation.register_abort_waker(&target_apid, {
        let matching = Arc::clone(&matching);
        Arc::new(move || {
            matching.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let _other_guard = cancellation.register_abort_waker(&other_apid, {
        let other = Arc::clone(&other);
        Arc::new(move || {
            other.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });

    cancellation.cancel(target_apid);

    assert_eq!(matching.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(other.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn cancellation_shutdown_wakes_all_registered_abort_wakers() {
    // Provider shutdown must wake every active ChatGPT WebSocket turn so workers
    // can return their normal canceled terminal path instead of waiting on idle
    // upstream sockets.
    let cancellation = Arc::new(CancellationState::default());
    let first_apid = tau_proto::AgentPromptId::from("ap-first");
    let second_apid = tau_proto::AgentPromptId::from("ap-second");
    let first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let second = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let _first_guard = cancellation.register_abort_waker(&first_apid, {
        let first = Arc::clone(&first);
        Arc::new(move || {
            first.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let _second_guard = cancellation.register_abort_waker(&second_apid, {
        let second = Arc::clone(&second);
        Arc::new(move || {
            second.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let apid = tau_proto::AgentPromptId::from("ap-drop");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let guard = cancellation.register_abort_waker(&apid, {
        let calls = Arc::clone(&calls);
        Arc::new(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    drop(guard);

    cancellation.cancel(apid);

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn minimal_prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test".into(),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test".into(),
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
    }
}

fn decode_frames(bytes: &[u8]) -> Vec<tau_proto::HarnessInputMessage> {
    let mut reader = tau_proto::HarnessInputReader::new(std::io::BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
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
            "ap-test",
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
    let tau_proto::Event::ProviderResponseUpdated(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(matches!(
        &update.status,
        Some(tau_proto::ProviderResponseStatusUpdate {
            clear_response: true,
            text,
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
            "ap-test",
            &prompt,
            &backend,
            tau_provider_chatgpt::common::LlmError::RepetitionDetected(repetition),
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
    let tau_proto::Event::ProviderResponseFinished(finished) = emit.event.as_ref() else {
        panic!("expected provider response finished: {:?}", emit.event);
    };
    assert_eq!(
        finished.stop_reason,
        tau_proto::ProviderStopReason::RepetitionDetected
    );
    assert!(finished.output_items.is_empty());
    assert!(finished.error.as_deref().unwrap_or_default().len() <= 520);
}
