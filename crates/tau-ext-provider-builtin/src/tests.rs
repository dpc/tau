use tau_provider_chat_completions::openrouter::OpenRouterProfile;

use super::*;

#[test]
fn compaction_output_finishes_as_normal_end_turn() {
    // Regression: server-side compaction is now represented by a durable output
    // item, not a special provider lifecycle stop reason.
    let output_items = [tau_proto::ContextItem::Compaction(
        tau_proto::OpaqueProviderItem(tau_proto::CborValue::Map(Vec::new())),
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
        tau_proto::ContextItem::Compaction(tau_proto::OpaqueProviderItem(
            tau_proto::CborValue::Map(Vec::new()),
        )),
        tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: "call-compact-tool".into(),
            name: tau_proto::ToolName::new("echo"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Null,
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
            "session-test",
            "ap-test",
            &prompt,
            &backend,
            tau_provider_chatgpt::common::LlmError::RepetitionDetected(repetition),
            None,
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
