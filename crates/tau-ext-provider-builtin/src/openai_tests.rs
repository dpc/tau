use std::io::{BufReader, Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tau_proto::{
    Effort, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    Verbosity,
};

use super::*;

/// Shared byte sink used by tests that run tau-client's writer thread.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the provider runtime.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    /// Returns a snapshot of bytes written so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn chatgpt_auth() -> OpenAiAuth {
    OpenAiAuth {
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account".to_owned()),
    }
}

fn model_ids(models: &[ProviderModelInfo]) -> Vec<String> {
    models.iter().map(|model| model.id.to_string()).collect()
}

fn decode_frames(bytes: &[u8]) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
}

fn encode_frames(frames: &[HarnessOutputMessage]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut writer = HarnessOutputWriter::new(&mut bytes);
        for frame in frames {
            writer.write_message(frame).expect("encode frame");
        }
        writer.flush().expect("flush frames");
    }
    bytes
}

fn live_event(recorded_at: u64, event: Event) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_live(tau_proto::UnixMicros::new(recorded_at), event)
}

fn replay_event(recorded_at: u64, event: Event) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_replay(tau_proto::UnixMicros::new(recorded_at), event)
}

fn input_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn session_dir(status: tau_proto::SessionDirStatus) -> tau_proto::HarnessSessionDir {
    tau_proto::HarnessSessionDir {
        session_id: "session-1".into(),
        path: std::path::PathBuf::from("/tmp/tau-test-session-1"),
        status,
    }
}

#[test]
fn retry_banner_emits_status_not_message_delta() {
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_retry_banner(
            "sp-retry",
            &tau_proto::AgentId::parse("main").expect("agent id"),
            &tau_proto::PromptOriginator::User,
            &mut writer,
            &common::LlmError::HttpStatus(500, "temporary".to_owned()),
            Duration::from_secs(1),
            1,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(Event::ProviderResponseUpdated(update)) = frames.first().and_then(input_event) else {
        panic!("expected provider response update frame: {frames:?}");
    };
    assert!(update.deltas.is_empty());
    assert!(matches!(
        update.status.as_ref(),
        Some(tau_proto::ProviderResponseStatusUpdate {
            text,
            clear_response: true,
        }) if text.contains("provider error")
    ));
}

fn model_id(provider: &str, model: &str) -> ModelId {
    ModelId::new(ProviderName::new(provider), ModelName::new(model))
}

fn prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "sp-1".into(),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        session_id: "session-1".into(),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: "hello".to_owned(),
                        }],
                        phase: None,
                    })],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: model_id(CHATGPT_PROVIDER_NAME, "gpt-5.5"),
        model_params: Default::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
    }
}

#[test]
fn chatgpt_profile_publishes_models_even_without_auth_tokens() {
    // Profile existence is the registration signal. Auth validity affects
    // prompt execution, not whether the registered account's models are visible.
    let models = models_for_auth(&OpenAiAuth::default());

    assert!(model_ids(&models).starts_with(&["chatgpt/gpt-5.5".to_owned()]));
}

#[test]
fn chatgpt_oauth_publishes_chatgpt_models() {
    // ChatGPT/Codex is a provider namespace named `chatgpt`; there is no
    // compatibility fallback to an `openai-codex` provider name.
    let models = models_for_auth(&chatgpt_auth());

    assert_eq!(
        model_ids(&models),
        vec![
            "chatgpt/gpt-5.5",
            "chatgpt/gpt-5.4",
            "chatgpt/gpt-5.4-mini",
            "chatgpt/gpt-5.3-codex"
        ]
    );
    assert!(models.iter().all(|model| model.supports_compaction));
}

#[test]
fn resolves_chatgpt_to_codex_responses_backend() {
    // ChatGPT is OAuth-backed and enables Codex-specific transport and replay
    // features owned by this provider slice.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());

    let config =
        resolve_responses_backend(&model_id(CHATGPT_PROVIDER_NAME, "gpt-5.4"), &mut profiles)
            .expect("chatgpt backend");

    assert_eq!(config.surface, responses::ResponsesSurface::ChatGpt);
    assert_eq!(config.base_url, tau_provider_chatgpt::DEFAULT_BASE_URL);
    assert_eq!(config.api_key, "access");
    assert_eq!(config.account_id.as_deref(), Some("account"));
    assert!(config.supports_websocket);
    assert!(config.supports_compaction);
    assert!(config.supports_phase);
    assert!(config.supports_encrypted_reasoning);
}

#[test]
fn chatgpt_phase_metadata_is_model_specific() {
    // The assistant `phase` field is only accepted by newer Codex model
    // families, so the hardcoded resolver must preserve the old whitelist.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());

    let old = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.2-codex"),
        &mut profiles,
    )
    .expect("old codex backend");
    let new = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.3-codex"),
        &mut profiles,
    )
    .expect("new codex backend");

    assert!(!old.supports_phase);
    assert!(new.supports_phase);
}

#[test]
fn xhigh_metadata_is_model_specific() {
    // The UI cycles through the provider-published effort list, so hardcoded
    // metadata must preserve xhigh only for model families that accept it.
    let models = models_for_auth(&chatgpt_auth());
    let ids_with_xhigh = models
        .iter()
        .filter(|model| model.efforts.contains(&Effort::XHigh))
        .map(|model| model.id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        ids_with_xhigh,
        vec![
            "chatgpt/gpt-5.5",
            "chatgpt/gpt-5.4",
            "chatgpt/gpt-5.3-codex"
        ]
    );
}

#[test]
fn verbosity_metadata_is_published_for_chatgpt_models() {
    // The provider snapshot is authoritative for UI cycling, so ChatGPT
    // models must publish the verbosity choices they accept.
    let models = models_for_auth(&chatgpt_auth());
    let gpt = models
        .iter()
        .find(|model| model.id.to_string() == "chatgpt/gpt-5.5")
        .expect("gpt-5.5 model");

    assert_eq!(
        gpt.verbosities,
        vec![Verbosity::Low, Verbosity::Medium, Verbosity::High]
    );
}

#[test]
fn prompt_workers_start_concurrently() {
    // Regression coverage for backend-agent parallelism: two accepted
    // provider prompts must both enter worker execution before the first
    // one finishes. A serial dispatcher would time out the first worker's
    // wait and never observe two active starts at once.
    let mut first = prompt();
    first.agent_prompt_id = "sp-par-1".into();
    let mut second = prompt();
    second.agent_prompt_id = "sp-par-2".into();
    let input = encode_frames(&[
        live_event(11, Event::AgentPromptCreated(first)),
        live_event(12, Event::AgentPromptCreated(second)),
    ]);
    let started = std::sync::Arc::new((Mutex::new((0_usize, 0_usize)), Condvar::new()));
    let executor_started = started.clone();
    let executor: PromptExecutor = std::sync::Arc::new(move |execution| {
        let agent_prompt_id = execution.job.agent_prompt_id.clone();
        let originator = execution.job.prompt.originator.clone();
        let (lock, cv) = &*executor_started;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut guard = lock.lock().expect("started lock");
        guard.0 += 1;
        guard.1 = guard.1.max(guard.0);
        cv.notify_all();
        while guard.0 < 2 {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                break;
            };
            let (next, wait) = cv.wait_timeout(guard, remaining).expect("wait for peer");
            guard = next;
            if wait.timed_out() {
                break;
            }
        }
        drop(guard);

        let mut writer = execution.frame_writer();
        write_prompt_submitted(&agent_prompt_id, &originator, &mut writer).expect("submitted");
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    agent_prompt_id.clone(),
                    tau_proto::AgentId::parse("agent-1").expect("valid test agent id"),
                    originator,
                    "done",
                ),
            )))
            .expect("finished");
        writer.flush().expect("flush fake response");

        let mut guard = lock.lock().expect("started lock");
        guard.0 -= 1;
        cv.notify_all();
    });

    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_inner_with_prompt_executor(
        Cursor::new(input),
        writer,
        profiles,
        move || prompt_profiles.clone(),
        2,
        executor,
    )
    .expect("run provider extension");

    let max_started = started.0.lock().expect("started lock").1;
    assert_eq!(max_started, 2, "both prompt workers should overlap");
    let frames = decode_frames(&output.bytes());
    let finished_count = frames
        .iter()
        .filter(|frame| matches!(input_event(frame), Some(Event::ProviderResponseFinished(_))))
        .count();
    assert_eq!(finished_count, 2);
}

/// Replayed prompt creation is historical state and must not repeat provider
/// side effects after the extension subscribes to live events.
#[test]
fn replayed_prompt_creation_does_not_start_executor_or_emit_prompt_events() {
    let input = encode_frames(&[
        replay_event(10, Event::AgentPromptCreated(prompt())),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    ]);
    let executor_calls = Arc::new(AtomicUsize::new(0));
    let executor_calls_for_worker = executor_calls.clone();
    let executor: PromptExecutor = Arc::new(move |_| {
        executor_calls_for_worker.fetch_add(1, Ordering::SeqCst);
    });

    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_inner_with_prompt_executor(
        Cursor::new(input),
        writer,
        profiles,
        move || prompt_profiles.clone(),
        1,
        executor,
    )
    .expect("run provider extension");

    assert_eq!(executor_calls.load(Ordering::SeqCst), 0);
    let frames = decode_frames(&output.bytes());
    assert!(
        frames.iter().all(|frame| {
            !matches!(
                input_event(frame),
                Some(
                    Event::ProviderPromptSubmitted(_)
                        | Event::ProviderResponseUpdated(_)
                        | Event::ProviderResponseFinished(_)
                )
            )
        }),
        "replayed prompt must not emit provider prompt effects: {frames:?}"
    );
}

/// Replayed session-directory facts are current-state catch-up and must still
/// control whether later live prompts may write provider diagnostics.
#[test]
fn replayed_session_dir_controls_live_prompt_debug_policy() {
    for (status, expected_debug) in [
        (tau_proto::SessionDirStatus::New, true),
        (tau_proto::SessionDirStatus::Ephemeral, false),
    ] {
        let input = encode_frames(&[
            replay_event(10, Event::HarnessSessionDir(session_dir(status))),
            live_event(11, Event::AgentPromptCreated(prompt())),
        ]);
        let observed_debug = Arc::new(Mutex::new(None));
        let observed_debug_for_worker = observed_debug.clone();
        let executor: PromptExecutor = Arc::new(move |execution| {
            *observed_debug_for_worker
                .lock()
                .expect("observed debug lock") = Some(execution.job.debug_provider_requests);
        });

        let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
        let prompt_profiles = profiles.clone();
        let writer = SharedWriter::default();
        run_inner_with_prompt_executor(
            Cursor::new(input),
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider extension");

        assert_eq!(
            *observed_debug.lock().expect("observed debug lock"),
            Some(expected_debug),
            "session status should map to debug policy"
        );
    }
}

#[test]
fn provider_startup_declares_exact_subscriptions_and_models_before_ready() {
    // Provider model snapshots need to reach the harness during startup so
    // model/role UI state is available immediately after all extensions are
    // ready.
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_auth(std::io::empty(), writer, chatgpt_auth()).expect("run provider extension");

    let frames = decode_frames(&output.bytes());
    assert!(
        matches!(
            &frames[0],
            HarnessInputMessage::Hello(hello)
                if hello.client_kind == ClientKind::Provider
                    && hello.client_name.as_str() == EXTENSION_NAME
        ),
        "first frame should be provider hello: {frames:?}"
    );
    let subscribe_frames = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        subscribe_frames.len(),
        1,
        "provider should emit one startup Subscribe frame: {frames:?}",
    );
    assert_eq!(
        subscribe_frames[0].selectors,
        [
            tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_PREWARM_REQUESTED),
            tau_proto::EventSelector::Exact(EventName::HARNESS_SESSION_DIR),
            tau_proto::EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
        "provider startup subscriptions must stay exact and must not include direct prompt routing",
    );

    let models_index = frames
        .iter()
        .position(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderModelsUpdated(updated))
                    if model_ids(&updated.models).starts_with(&["chatgpt/gpt-5.5".to_owned()])
            )
        })
        .unwrap_or_else(|| panic!("startup frames should announce provider models: {frames:?}"));
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .unwrap_or_else(|| panic!("startup frames should end with Ready: {frames:?}"));
    assert!(
        models_index < ready_index,
        "provider models must be announced before Ready: {frames:?}",
    );
}

#[test]
fn direct_prompt_request_with_missing_backend_is_closed_with_error() {
    // Direct provider routing must never leave the harness waiting forever,
    // even if a prompt reaches this extension without usable credentials.
    let input = encode_frames(&[
        live_event(11, Event::AgentPromptCreated(prompt())),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    ]);
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_auth(Cursor::new(input), writer, OpenAiAuth::default())
        .expect("run provider extension");

    let frames = decode_frames(&output.bytes());
    let submitted = frames.iter().position(|frame| {
        matches!(
            input_event(frame),
            Some(Event::ProviderPromptSubmitted(submitted))
                if submitted.agent_prompt_id.as_str() == "sp-1"
        )
    });
    let finished = frames.iter().position(|frame| {
        matches!(
            input_event(frame),
            Some(Event::ProviderResponseFinished(finished))
                if finished.agent_prompt_id.as_str() == "sp-1"
                    && finished.stop_reason == ProviderStopReason::Error
                    && finished.output_items.is_empty()
                    && finished.error.as_deref()
                        == Some("cannot resolve provider backend for: chatgpt/gpt-5.5")
        )
    });
    let submitted = submitted.expect("prompt submitted event");
    let finished = finished.expect("missing-backend response finished event");
    assert!(submitted < finished, "submission should precede finish");
}
