use tau_provider_chat_completions::openrouter::OpenRouterProfile;

use super::*;

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
        raw_context_window: 258_400,
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
    let mut retry = RecordingRetrySleeper;
    let runtime = ChatGptRuntime::new();
    let mut bytes = Vec::new();
    let mut writer = PeerOutputWriter::new(&mut bytes);

    let outcome = handle_prompt(
        "ap-ws-terminal",
        &config,
        &prompt,
        &mut writer,
        &mut retry,
        ChatGptPromptExecutionContext {
            debug_provider_requests: false,
            runtime: &runtime,
        },
        &mut |_| {},
    )
    .expect("prompt handler should classify provider error");
    assert!(
        outcome.is_some(),
        "ambiguous WebSocket failures keep the logical prompt pending"
    );

    assert_eq!(
        counts
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "terminal WS error must not be produced by HTTP/SSE fallback"
    );
    assert!(
        decode_frames(&bytes).iter().all(|frame| !matches!(
            frame,
            tau_proto::HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    tau_proto::Event::ProviderResponseFinished(_)
                )
        )),
        "retryable attempts must not close the logical prompt"
    );
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
    // Under `DESIGN-tau-ext-provider-builtin-profile-ownership`, user-configured
    // Chat Completions namespaces publish exactly their configured models and
    // reject unknown model ids instead of falling back to a ChatGPT/Codex backend.
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
    let prompt_id = tau_proto::AgentPromptId::new("ap-late-retry");
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
    let target_apid = tau_proto::AgentPromptId::from("ap-target");
    let other_apid = tau_proto::AgentPromptId::from("ap-other");
    let matching = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let other = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let cancel_generation = cancellation.retry_generation();
    let _matching_guard = cancellation.register_abort_waker(&target_apid, cancel_generation, {
        let matching = Arc::clone(&matching);
        Arc::new(move || {
            matching.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let _other_guard = cancellation.register_abort_waker(&other_apid, cancel_generation, {
        let other = Arc::clone(&other);
        Arc::new(move || {
            other.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let first_apid = tau_proto::AgentPromptId::from("ap-first");
    let second_apid = tau_proto::AgentPromptId::from("ap-second");
    let first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let second = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let initial_generation = cancellation.retry_generation();

    let _first_guard = cancellation.register_abort_waker(&first_apid, initial_generation, {
        let first = Arc::clone(&first);
        Arc::new(move || {
            first.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let _second_guard = cancellation.register_abort_waker(&second_apid, initial_generation, {
        let second = Arc::clone(&second);
        Arc::new(move || {
            second.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let prompt_id = tau_proto::AgentPromptId::from("ap-late-registration");
    let stale_generation = cancellation.retry_generation();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    cancellation.cancel_all();
    let _guard = cancellation.register_abort_waker(&prompt_id, stale_generation, {
        let calls = Arc::clone(&calls);
        Arc::new(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let first_apid = tau_proto::AgentPromptId::from("ap-first");
    let second_apid = tau_proto::AgentPromptId::from("ap-second");
    let first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let second = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let cancel_generation = cancellation.retry_generation();
    let _first_guard = cancellation.register_abort_waker(&first_apid, cancel_generation, {
        let first = Arc::clone(&first);
        Arc::new(move || {
            first.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let _second_guard = cancellation.register_abort_waker(&second_apid, cancel_generation, {
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

    let guard = cancellation.register_abort_waker(&apid, cancellation.retry_generation(), {
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
        operation: tau_proto::PromptOperation::Inference,
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

/// Ensures the built-in ChatGPT/Codex emission boundary does not suppress
/// public stats-only streams that have no displayable text or compaction.
#[test]
fn chatgpt_stream_update_emits_response_stats_without_text_deltas() {
    let prompt = minimal_prompt();
    let mut state = common::StreamState::new();
    state
        .tool_call_at_mut(0, tau_proto::ToolType::Custom)
        .arguments_json
        .push_str("raw custom input");
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut delta_emitter = common::StreamDeltaEmitter::default();
        emit_chatgpt_stream_update(
            prompt.agent_prompt_id.as_str(),
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
            },
            &mut writer,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(tau_proto::HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected provider response update frame: {frames:?}");
    };
    let tau_proto::Event::ProviderResponseUpdated(update) = emit.event.as_ref() else {
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

/// Ensures ChatGPT/Codex provider progress frames publish the first streamed
/// chunk promptly, then follow provider-prompt cadence instead of emitting once
/// per upstream chunk or byte change.
#[test]
fn chatgpt_response_update_emitter_rate_limits_non_terminal_updates() {
    let prompt = minimal_prompt();
    let mut state = common::StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: prompt.agent_prompt_id.as_str(),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        state.append_message_delta_at(0, "hel");
        emitter.emit_at(&target, &state, &mut writer, start, false);
        state.append_message_delta_at(0, "lo");
        state
            .tool_call_at_mut(1, tau_proto::ToolType::Custom)
            .arguments_json
            .push_str("raw custom input");
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
                tau_proto::Event::ProviderResponseUpdated(update) => Some(update),
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
        })
    );
}

/// Ensures due ChatGPT/Codex response samples are emitted even when no bytes
/// changed, so provider `previous` always names the last emitted stats point.
#[test]
fn chatgpt_response_update_emitter_emits_due_stats_only_sample() {
    let prompt = minimal_prompt();
    let state = common::StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: prompt.agent_prompt_id.as_str(),
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
                tau_proto::Event::ProviderResponseUpdated(update) => Some(update),
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
        })
    );
}

/// Ensures a due zero-byte idle sample does not consume the first non-empty
/// bypass for streamed output, while later non-terminal bytes still obey the
/// one-second cadence.
#[test]
fn chatgpt_response_update_emitter_emits_first_bytes_after_idle_sample_promptly() {
    let prompt = minimal_prompt();
    let mut state = common::StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: prompt.agent_prompt_id.as_str(),
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
        state.append_message_delta_at(0, "hi");
        emitter.emit_at(
            &target,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        state.append_message_delta_at(0, "!");
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
                tau_proto::Event::ProviderResponseUpdated(update) => Some(update),
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
    let mut state = common::StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: prompt.agent_prompt_id.as_str(),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        state
            .tool_call_at_mut(1, tau_proto::ToolType::Custom)
            .arguments_json
            .push_str("raw custom input");
        emitter.emit_at(&target, &state, &mut writer, start, false);
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            tau_proto::HarnessInputMessage::Emit(emit) => match *emit.event {
                tau_proto::Event::ProviderResponseUpdated(update) => Some(update),
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
    let mut state = common::StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        let target = ResponseUpdateTarget {
            agent_prompt_id: prompt.agent_prompt_id.as_str(),
            agent_id: &prompt.agent_id,
            originator: &prompt.originator,
        };
        state.append_message_delta_at(0, "hel");
        emitter.emit_at(&target, &state, &mut writer, start, false);
        state.append_message_delta_at(0, "lo");
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
                tau_proto::Event::ProviderResponseUpdated(update) => Some(update),
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
    assert!(matches!(established, Event::ProviderQuotaReplace(_)));
    let (epoch, fetch_sequence) = quota
        .begin_fetch(&provider)
        .expect("valid quota test value");
    let rolling = tau_provider_chatgpt::quota::RollingQuotaObservation {
        windows: vec![tau_provider_chatgpt::quota::QuotaWindowObservation {
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
        Some(Event::ProviderQuotaPatch(_))
    ));
    let full = tau_provider_chatgpt::quota::FullQuotaSnapshot {
        windows: vec![tau_provider_chatgpt::quota::QuotaWindowObservation {
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
    let Event::ProviderQuotaReplace(replaced) = quota
        .finish_fetch(provider, epoch, fetch_sequence, full, 2_000_000_001_000)
        .expect("valid quota test value")
    else {
        panic!("expected replacement");
    };
    assert_eq!(replaced.windows[0].used_basis_points, 6_000);
    assert_eq!(replaced.route_bindings.len(), 1);
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
    let window =
        |limit_id: &str, used_basis_points| tau_provider_chatgpt::quota::QuotaWindowObservation {
            limit_id: tau_proto::ProviderQuotaLimitId::parse(limit_id).expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
            used_basis_points,
            window_seconds: Some(604_800),
            reset_at_unix_seconds: Some(2_100_000_000),
            remaining_seconds: Some(500_000),
        };
    let full = tau_provider_chatgpt::quota::FullQuotaSnapshot {
        windows: vec![window("codex", 4_400), window("codex_bengalfox", 0)],
    };
    let Event::ProviderQuotaReplace(replaced) = quota
        .finish_fetch(provider, epoch, fetch_sequence, full, 2_000_000_000_000)
        .expect("full quota replacement")
    else {
        panic!("expected quota replacement");
    };
    assert_eq!(replaced.windows.len(), 2);
    assert!(replaced.route_bindings.is_empty());

    let observation = tau_provider_chatgpt::quota::parse_ws_event(
        r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":45,"window_minutes":10080,"reset_at":2100000000}}}"#,
    )
    .expect("official nameless turn event");
    let Event::ProviderQuotaPatch(patch) = quota
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
                tau_provider_chatgpt::quota::FullQuotaSnapshot::default(),
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
        let observation = tau_provider_chatgpt::quota::RollingQuotaObservation {
            windows: vec![tau_provider_chatgpt::quota::QuotaWindowObservation {
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
    let overflow = tau_provider_chatgpt::quota::RollingQuotaObservation {
        windows: vec![tau_provider_chatgpt::quota::QuotaWindowObservation {
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
    let rolling =
        |prefix: &str, index: usize| tau_provider_chatgpt::quota::RollingQuotaObservation {
            windows: vec![tau_provider_chatgpt::quota::QuotaWindowObservation {
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
    let full = tau_provider_chatgpt::quota::FullQuotaSnapshot {
        windows: (0..32)
            .map(
                |index| tau_provider_chatgpt::quota::QuotaWindowObservation {
                    limit_id: tau_proto::ProviderQuotaLimitId::parse(format!("full_{index}"))
                        .expect("pool id"),
                    window_id: tau_proto::ProviderQuotaWindowId::parse("primary")
                        .expect("window id"),
                    used_basis_points: 200,
                    window_seconds: Some(604_800),
                    reset_at_unix_seconds: Some(2_100_000_000),
                    remaining_seconds: Some(300_000),
                },
            )
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
