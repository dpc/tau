use super::*;

fn unique_temp_state_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tau-provider-chat-completions-state-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn decode_frames(bytes: &[u8]) -> Vec<HarnessInputMessage> {
    let mut reader = tau_proto::HarnessInputReader::new(std::io::BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
}

fn provider() -> ChatCompletionsProvider {
    ChatCompletionsProvider {
        base_url: "https://api.openai.com/v1".to_owned(),
        api_key: "key".to_owned(),
        models: vec![ChatCompletionsModel {
            id: ModelName::new("gpt-4o"),
            display_name: None,
            context_window: 128_000,
            compat: None,
            tags: Vec::new(),
        }],
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        tags: Vec::new(),
        compat: ChatCompletionsCompat::openai_defaults(),
    }
}

#[test]
fn debug_provider_request_dir_requires_existing_session_dir() {
    // Provider diagnostics are allowed to create their own debug subdirectory,
    // but must not create durable per-session roots for ephemeral sessions.
    let state_dir = unique_temp_state_dir("missing-session");
    let session_id = "session-missing";

    assert!(debug_provider_request_dir_in(&state_dir, session_id, true).is_none());
    assert!(
        !state_dir.join("sessions").join(session_id).exists(),
        "missing session dir should not be created"
    );
}

#[test]
fn debug_provider_request_dir_returns_debug_dir_for_existing_session() {
    // Durable sessions create their session directory before provider calls; in
    // that case provider diagnostics can write under the standard debug path.
    let state_dir = unique_temp_state_dir("existing-session");
    let session_id = "session-existing";
    let session_dir = state_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).expect("create durable session dir");

    assert_eq!(
        debug_provider_request_dir_in(&state_dir, session_id, true),
        Some(session_dir.join("debug").join("provider-requests"))
    );
}

#[test]
fn debug_provider_request_dir_rejects_ephemeral_session_with_existing_dir() {
    // Explicit session persistence state wins over filesystem shape: an
    // ephemeral current session can reuse an id that has an old durable
    // directory, and provider diagnostics must still stay disabled.
    let state_dir = unique_temp_state_dir("ephemeral-reuse");
    let session_id = "session-reused";
    let session_dir = state_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).expect("create old durable session dir");

    assert!(debug_provider_request_dir_in(&state_dir, session_id, false).is_none());
}

/// Ensures provider-wide and model-local model tags are both published once so
/// harness policy can reason about OpenAI-compatible model capabilities.
#[test]
fn models_for_provider_unions_provider_and_model_tags() {
    let mut provider = provider();
    provider.tags = vec![ModelTag::new("tools:function-json")];
    provider.models[0].tags = vec![
        ModelTag::new("tools:function-json"),
        ModelTag::new("shell:custom"),
    ];

    let models = models_for_provider(&ProviderName::new("openai"), &provider);

    assert_eq!(
        models[0].tags,
        vec![
            ModelTag::new("tools:function-json"),
            ModelTag::new("shell:custom")
        ]
    );
}

fn resolved_provider(provider: &ChatCompletionsProvider) -> ResolvedProvider {
    ResolvedProvider {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: provider.max_output_tokens,
        extra_body: provider.extra_body.clone(),
        compat: provider.compat,
    }
}

/// Ensures Chat Completions streaming progress emits append deltas rather than
/// full accumulated assistant/reasoning snapshots.
#[test]
fn stream_delta_emitter_emits_append_deltas() {
    let mut state = StreamState::new();
    let mut emitter = StreamDeltaEmitter::default();

    state
        .append_assistant_text_delta("hel")
        .expect("stream event should apply");
    state
        .append_reasoning_delta("think")
        .expect("stream event should apply");
    assert_eq!(
        emitter.deltas(&state),
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "hel".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Full,
                text: "think".to_owned(),
            },
        ]
    );

    state
        .append_assistant_text_delta("lo")
        .expect("stream event should apply");
    assert_eq!(
        emitter.deltas(&state),
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 2,
            text: "lo".to_owned(),
            phase: None,
        }]
    );
}

/// Ensures rare provider corrections do not produce corrupt suffix deltas; the
/// final complete response is responsible for correcting the UI.
#[test]
fn stream_delta_emitter_drops_non_prefix_corrections() {
    let mut state = StreamState::new();
    let mut emitter = StreamDeltaEmitter::default();

    state
        .append_assistant_text_delta("abcd")
        .expect("stream event should apply");
    assert_eq!(
        emitter.deltas(&state),
        vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "abcd".to_owned(),
            phase: None,
        }]
    );

    let OutputItemAccumulator::Message(text) = &mut state.output_items[0] else {
        panic!("expected message output item");
    };
    *text = "wxyz12".to_owned();

    assert!(
        emitter.deltas(&state).is_empty(),
        "non-prefix rewrite must not emit a misleading suffix"
    );
}

/// Empty-response retry diagnostics are provider status text, not assistant
/// deltas, so they must not pollute live assistant accumulation.
#[test]
fn empty_response_retry_emits_status_not_message_delta() {
    let mut bytes = Vec::new();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        emit_empty_response_retry_update(
            &AgentPromptId::from("sp-retry"),
            &prompt(),
            1,
            &mut writer,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected emitted retry update frame: {frames:?}");
    };
    let Event::ProviderResponseUpdated(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(update.deltas.is_empty());
    assert!(matches!(
        update.status.as_ref(),
        Some(tau_proto::ProviderResponseStatusUpdate {
            text,
            clear_response: true,
        }) if text.contains("provider returned an empty response")
    ));
}

fn prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test".into(),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test".into(),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(tau_proto::MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: "hello".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
    }
}

#[test]
fn publishes_configured_models_for_registered_provider() {
    // Built-in provider profiles derive the Tau provider namespace from the
    // profile filename; the Chat Completions backend only turns one registered
    // profile into model publication records.
    let models = models_for_provider(&ProviderName::new("openai"), &provider());

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.to_string(), "openai/gpt-4o");
    assert!(!models[0].supports_compaction);
}

#[test]
fn provider_with_reasoning_effort_publishes_effort_levels() {
    // Role effort selection is clamped to provider-advertised levels. OpenAI
    // compatible profiles that opt into reasoning_effort must publish the
    // corresponding choices.
    let models = models_for_provider(&ProviderName::new("openai"), &provider());

    assert!(models[0].efforts.contains(&tau_proto::Effort::High));
    assert!(models[0].efforts.contains(&tau_proto::Effort::Off));
}

#[test]
fn tool_result_text_uses_structured_status_headers() {
    // Chat Completions and Responses API providers should expose identical
    // provider-facing text for non-success tool results, so model behavior
    // does not depend on the selected OpenAI-compatible API surface.
    let output = tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("body".into()));

    assert_eq!(
        tool_result_text(
            ToolResultStatus::Error {
                message: "failed".to_owned(),
            },
            &output,
        ),
        "error: failed\n\nbody",
    );
    assert_eq!(
        tool_result_text(
            ToolResultStatus::Cancelled {
                reason: "stopped".to_owned(),
            },
            &output,
        ),
        "cancelled: stopped\n\n",
    );
}

#[test]
fn reasoning_content_is_persisted_and_replayed_with_tool_call() {
    // Local reasoning Chat Completions servers may require the assistant's
    // reasoning_content to be replayed on the assistant tool-call message that
    // precedes tool results. Dropping it can break tool-call continuation after
    // the tool response is appended.
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "reasoning_content": "need current date" },
                "finish_reason": null
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": "shell", "arguments": "{\"command\":\"date\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");
    let items = state.output_items();
    assert!(matches!(items[0], ContextItem::ReasoningText(_)));
    assert!(matches!(items[1], ContextItem::ToolCall(_)));

    let mut replay = prompt();
    replay.context = tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: items,
                usage: None,
            },
        )],
    };
    let provider = provider();
    let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["messages"][0]["role"], "assistant");
    assert_eq!(
        json["messages"][0]["reasoning_content"],
        "need current date"
    );
    assert_eq!(
        json["messages"][0]["tool_calls"][0]["function"]["name"],
        "shell"
    );
}

/// Ensures Chat Completions preserves provider-wire function-call argument JSON
/// through parsing and replay so cache identity is not changed by
/// reserialization.
#[test]
fn tool_call_replay_preserves_raw_function_arguments_json() {
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-raw",
                        "type": "function",
                        "function": { "name": "shell", "arguments": raw_arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");

    let items = state.output_items();
    let ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected persisted tool call");
    };
    assert_eq!(call.raw_arguments_json.as_deref(), Some(raw_arguments));

    let mut replay = prompt();
    replay.context = tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: items,
                usage: None,
            },
        )],
    };
    let provider = provider();
    let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(
        json["messages"][0]["tool_calls"][0]["function"]["arguments"],
        raw_arguments
    );
}

/// Ensures old persisted Chat Completions tool calls without a raw JSON sidecar
/// still replay by serializing the parsed CBOR semantic arguments.
#[test]
fn tool_call_replay_falls_back_to_parsed_arguments_when_raw_json_missing() {
    let mut replay = prompt();
    replay.context = tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-fallback".into(),
                    name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Map(vec![(
                        tau_proto::CborValue::Text("command".to_owned()),
                        tau_proto::CborValue::Text("date".to_owned()),
                    )]),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                usage: None,
            },
        )],
    };
    let provider = provider();
    let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(
        json["messages"][0]["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"date\"}"
    );
}

#[test]
fn replay_coalesces_assistant_text_and_tool_calls_in_stream_order() {
    // A single Chat Completions assistant turn can contain reasoning, visible
    // content, and multiple tool calls. Tau stores those as ordered context
    // items, so replay must rebuild one assistant message instead of splitting
    // the content and tool calls into separate turns.
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "need two facts",
                    "content": "I'll check.",
                },
                "finish_reason": null
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 1,
                        "id": "call-b",
                        "type": "function",
                        "function": { "name": "grep", "arguments": "{\"pattern\":\"b\"}" }
                    }, {
                        "index": 0,
                        "id": "call-a",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");
    let items = state.output_items();
    assert!(matches!(items[0], ContextItem::ReasoningText(_)));
    assert!(matches!(items[1], ContextItem::Message(_)));
    assert!(matches!(items[2], ContextItem::ToolCall(_)));
    assert!(matches!(items[3], ContextItem::ToolCall(_)));

    let mut replay = prompt();
    replay.context = tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: items,
                usage: None,
            },
        )],
    };
    let provider = provider();
    let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(json["messages"][0]["role"], "assistant");
    assert_eq!(json["messages"][0]["content"], "I'll check.");
    assert_eq!(json["messages"][0]["reasoning_content"], "need two facts");
    assert_eq!(json["messages"][0]["tool_calls"][0]["id"], "call-b");
    assert_eq!(json["messages"][0]["tool_calls"][1]["id"], "call-a");
}

#[test]
fn think_tags_are_persisted_as_reasoning_content() {
    // Some local servers expose reasoning inside content with <think> tags
    // instead of a dedicated reasoning_content delta. Preserve the hidden text
    // for replay while keeping it out of visible assistant content.
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "content": "<think>secret plan</think>visible" },
                "finish_reason": "stop"
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");
    let items = state.output_items();
    assert!(matches!(items[0], ContextItem::ReasoningText(_)));
    let ContextItem::Message(message) = &items[1] else {
        panic!("expected visible assistant message");
    };
    assert!(matches!(
        &message.content[0],
        ContentPart::Text { text } if text == "visible"
    ));
}

#[test]
fn provider_config_rejects_unknown_fields() {
    // Chat Completions profiles are user-authored provider config. Unknown
    // fields should fail fast instead of silently disabling an intended setting.
    let error = serde_json::from_value::<ChatCompletionsProvider>(serde_json::json!({
        "base_url": "https://api.openai.com/v1",
        "models": [{ "id": "gpt-4o", "extra": true }],
    }))
    .expect_err("model entry should reject unknown fields");

    assert!(error.to_string().contains("unknown field"), "got: {error}");
}

#[test]
fn chat_request_sets_default_max_tokens_for_generic_providers() {
    // llama.cpp and other local Chat Completions servers can default to a tiny
    // output cap when clients omit max_tokens. Generic profiles should send a
    // Tau cap explicitly so tool-heavy turns do not stop after a preamble.
    let mut provider = provider();
    provider.compat.max_completion_tokens = false;
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(json.get("max_completion_tokens").is_none());
}

#[test]
fn chat_request_sends_slashy_model_ids_unchanged() {
    // Provider-native model ids can contain `/`; Tau's provider namespace is
    // separated at the `ModelId` layer, not in the Chat Completions request.
    let mut provider = provider();
    provider.models[0].id = ModelName::new("anthropic/claude-sonnet-4");
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["model"], "anthropic/claude-sonnet-4");
}

#[test]
fn chat_request_uses_max_completion_tokens_when_enabled() {
    // OpenAI-compatible reasoning models can reject the legacy max_tokens name.
    // The existing compatibility switch now selects the modern wire field for
    // the same Tau-owned output cap.
    let provider = provider();
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["max_completion_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(json.get("max_tokens").is_none());
}

#[test]
fn extra_body_output_token_cap_overrides_automatic_cap() {
    // Provider profiles can still use non-standard caps or deliberately lower
    // limits through extra_body. Avoid serializing a duplicate max token field
    // when the profile already owns either Chat Completions cap spelling.
    let mut provider = provider();
    provider.compat.max_completion_tokens = false;
    provider
        .extra_body
        .insert("max_tokens".to_owned(), serde_json::json!(128));
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["max_tokens"], 128);
    assert!(json.get("max_completion_tokens").is_none());
}

#[test]
fn length_finish_reason_maps_to_length_stop_reason() {
    // Regression coverage for diagnosing local-server premature stops: a raw
    // Chat Completions `finish_reason: length` is distinct from a normal
    // end-turn and should survive into Tau's provider response metadata.
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "length"
            }]
        }),
        &mut |_| {},
    )
    .expect("stream event should apply");

    assert_eq!(state.stop_reason, ProviderStopReason::Length);
}

#[test]
fn empty_end_turn_is_rejected_before_harness_completion() {
    // Regression: some local Chat Completions servers occasionally answer a
    // tool-result follow-up with `finish_reason: stop`, usage, and no content
    // or tool calls. Treating that as a normal turn silently marks the agent as
    // done with an empty message, so the backend must surface it as retryable.
    let state = StreamState::new();

    assert!(matches!(
        ensure_non_empty_end_turn(state),
        Err(LlmError::EmptyResponse)
    ));
}

#[test]
fn non_empty_end_turn_is_accepted() {
    // A normal assistant text response should not be affected by the empty-turn
    // guard.
    let mut state = StreamState::new();
    state
        .append_assistant_text_delta("done")
        .expect("stream event should apply");

    assert!(ensure_non_empty_end_turn(state).is_ok());
}

#[test]
fn tool_call_turn_is_accepted_without_text() {
    // Tool-call turns often have no assistant text; they are valid as long as a
    // parsed tool call is present.
    let mut state = StreamState::new();
    state.stop_reason = ProviderStopReason::ToolCalls;
    let call = state.tool_call_at_mut(0);
    call.id = "call-1".to_owned();
    call.name = "shell".to_owned();
    call.arguments = "{}".to_owned();

    assert!(ensure_non_empty_end_turn(state).is_ok());
}

#[test]
fn repeated_assistant_content_delta_aborts_stream_event() {
    // Ensures Chat Completions catches tight assistant text loops while parsing
    // stream deltas, before they become final assistant output.
    let mut state = StreamState::new();
    let result = apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "content": ".".repeat(1024) },
                "finish_reason": null
            }]
        }),
        &mut |_| {},
    );
    assert!(matches!(result, Err(LlmError::RepetitionDetected(_))));
    assert!(state.output_items().is_empty());
}

#[test]
fn repeated_tool_argument_delta_aborts_stream_event() {
    // Ensures Chat Completions catches tight exact function-argument loops from
    // providers before accepting the generated argument suffix.
    let mut state = StreamState::new();
    let result = apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "type": "function",
                        "function": { "name": "shell", "arguments": "_clone".repeat(180) }
                    }]
                },
                "finish_reason": null
            }]
        }),
        &mut |_| {},
    );
    assert!(matches!(result, Err(LlmError::RepetitionDetected(_))));
    let OutputItemAccumulator::ToolCall(call) = &state.output_items[0] else {
        panic!("tool call accumulator should exist");
    };
    assert!(call.arguments.is_empty());
}

#[test]
fn repeated_reasoning_delta_aborts_stream_event() {
    // Ensures Chat Completions catches tight reasoning loops independently from
    // assistant text and before accepting the reasoning suffix.
    let mut state = StreamState::new();
    let result = apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "reasoning_content": ".".repeat(1024) },
                "finish_reason": null
            }]
        }),
        &mut |_| {},
    );
    assert!(matches!(result, Err(LlmError::RepetitionDetected(_))));
    assert!(state.output_items().is_empty());
}

#[test]
fn repetition_error_finishes_with_clear_response_contract() {
    // The Chat Completions provider must clear transient output and then finish
    // with an empty repetition-detected response instead of retrying or shipping
    // partial model text.
    let prompt = prompt();
    let repetition = tau_provider::StreamRepetition {
        key: tau_provider::StreamRepetitionKey::AssistantText { output_index: 0 },
        mode: tau_provider::RepetitionMode::Fragment,
        snippet: ".".to_owned(),
    };
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_repetition_detected_update(&"ap-test".into(), &prompt, &repetition, &mut writer);
    }
    let frames = decode_frames(&bytes);
    let Some(HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected emitted repetition update frame: {frames:?}");
    };
    let Event::ProviderResponseUpdated(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(matches!(
        &update.status,
        Some(tau_proto::ProviderResponseStatusUpdate {
            clear_response: true,
            text,
        }) if text.contains("repetition detected")
    ));

    let finished = finish_error(
        &"ap-test".into(),
        &prompt,
        &ResolvedProvider {
            base_url: "https://example.invalid".to_owned(),
            api_key: String::new(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            extra_body: BTreeMap::new(),
            compat: ChatCompletionsCompat::default(),
        },
        LlmError::RepetitionDetected(repetition),
    );
    assert_eq!(finished.stop_reason, ProviderStopReason::RepetitionDetected);
    assert!(finished.output_items.is_empty());
    assert!(finished.error.as_deref().unwrap_or_default().len() <= 520);
}
