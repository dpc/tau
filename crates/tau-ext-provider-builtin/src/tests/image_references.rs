use super::*;
use crate::image_resolution::{State, tests as resolver_fixture};

/// Selects an audited image route with an inert loopback endpoint; failures
/// must happen before it can try to submit a model request.
fn image_backend() -> PromptBackend {
    PromptBackend::Responses(
        tau_provider_codex::resolved_config_for_provider_model(
            &ProviderName::new("chatgpt"),
            &ModelName::new("gpt-5.4"),
            tau_provider_codex::ResolvedCredentials::new("unused".into(), None),
            CodexMode::Standard,
        )
        .with_test_base_url("http://127.0.0.1:1".into()),
    )
}

/// Supplies reference-bearing history without any upload or actual image bytes.
fn referenced_context() -> tau_proto::PromptContext {
    tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    call_id: "image".into(),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
                    presentation: Default::default(),
                    provider_content: vec![tau_proto::ToolResultContentPart::ImageReference(
                        tau_proto::ImageReference {
                            blob: tau_proto::ArtifactDescriptor::new(
                                tau_proto::ArtifactKey::from_digest(&"ab".repeat(32)).expect("key"),
                                0,
                            )
                            .expect("descriptor"),
                            media_type: tau_proto::ImageMediaType::Png,
                            width: 1,
                            height: 1,
                            detail: tau_proto::ImageDetail::High,
                        },
                    )],
                }],
            },
        )],
    }
}

/// A sticky cancellation fixture with no asynchronous source.
struct Aborted;

impl TurnAbort for Aborted {
    fn is_aborted(&mut self) -> bool {
        true
    }

    fn register_waker(
        &mut self,
        _: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(NoopAbortWaker)
    }
}

/// Ordinary and standalone dispatch must terminalize unresolved refs, never
/// retry, emit Artifact RPC, or proceed into the selected backend.
#[test]
fn unresolved_image_reference_terminalizes_before_backend_dispatch() {
    for operation in [
        tau_proto::PromptOperation::Inference,
        tau_proto::PromptOperation::StandaloneCompaction,
    ] {
        let mut prompt = minimal_prompt();
        prompt.operation = operation;
        prompt.context = referenced_context();
        let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
        let backend = image_backend();
        let mut bytes = Vec::new();
        let retry = handle_prompt_backend(
            &prompt.agent_prompt_id,
            &backend,
            &prompt,
            &mut tau_proto::PeerOutputWriter::new(&mut bytes),
            &mut RecordingRetrySleeper,
            ChatGptPromptExecutionContext {
                image_resolution: None,
                debug_provider_requests: false,
                runtime: &runtime,
                prior_backend: None,
                logical_attempt: tau_provider_codex::LogicalAttempt::new(3),
                compact_route_unavailable: &|_| panic!("no backend"),
            },
            &mut |_| panic!("no quota request"),
        )
        .expect("terminal report");
        assert!(retry.is_none());
        let frames = decode_frames(&bytes);
        assert_eq!(frames.len(), 1, "no Artifact or other protocol work");
        let HarnessInputMessage::Emit(emit) = &frames[0] else {
            panic!("terminal");
        };
        let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
            panic!("finished");
        };
        assert_eq!(
            finished.failure_kind,
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        );
        assert_eq!(
            finished.provider_attempt,
            tau_provider_codex::LogicalAttempt::new(3).provider_attempt()
        );
        assert!(finished.output_items.is_empty());
        assert!(finished.backend.is_none());
        assert!(finished.usage.is_none());
        assert!(
            !serde_json::to_string(&frames)
                .expect("frames")
                .contains("abababab")
        );
    }
}

/// Cancellation remains authoritative over unresolved input and invents no
/// failure category.
#[test]
fn unresolved_image_reference_preserves_cancellation_precedence() {
    let mut prompt = minimal_prompt();
    prompt.context = referenced_context();
    let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
    let mut bytes = Vec::new();
    let retry = handle_prompt_backend(
        &prompt.agent_prompt_id,
        &image_backend(),
        &prompt,
        &mut tau_proto::PeerOutputWriter::new(&mut bytes),
        &mut Aborted,
        ChatGptPromptExecutionContext {
            image_resolution: None,
            debug_provider_requests: false,
            runtime: &runtime,
            prior_backend: None,
            logical_attempt: tau_provider_codex::LogicalAttempt::new(1),
            compact_route_unavailable: &|_| {},
        },
        &mut |_| {},
    )
    .expect("canceled");
    assert!(retry.is_none());
    let frames = decode_frames(&bytes);
    assert_eq!(frames.len(), 1);
    let HarnessInputMessage::Emit(emit) = &frames[0] else {
        panic!("terminal");
    };
    let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
        panic!("finished");
    };
    assert_eq!(finished.failure_kind, None);
    assert_eq!(finished.error.as_deref(), Some("(cancelled by harness)"));
}

/// Dedicated prewarm bypasses ordinary dispatch, so it needs its own rejection.
#[test]
fn unresolved_image_reference_fails_prewarm_without_runtime_work() {
    let prompt = minimal_prompt();
    let prewarm = tau_proto::AgentPromptPrewarmRequested {
        agent_id: prompt.agent_id,
        session_id: prompt.session_id,
        system_prompt: String::new(),
        context: referenced_context(),
        tools: Vec::new(),
        model: Some(prompt.model),
        hosted_tools: Vec::new(),
        model_params: Default::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
    };
    let config = tau_provider_codex::resolved_config_for_provider_model(
        &ProviderName::new("chatgpt"),
        &ModelName::new("gpt-5.4"),
        tau_provider_codex::ResolvedCredentials::new("unused".into(), None),
        CodexMode::Standard,
    )
    .with_test_base_url("http://127.0.0.1:1".into());
    let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
    assert_eq!(
        handle_resolved_prewarm(
            &prewarm,
            &config,
            &runtime,
            None,
            false,
            &mut RecordingRetrySleeper,
            None,
        ),
        tau_proto::ProviderCacheRefreshStatus::Failed
    );
    assert_eq!(
        handle_resolved_prewarm(&prewarm, &config, &runtime, None, false, &mut Aborted, None),
        tau_proto::ProviderCacheRefreshStatus::Cancelled
    );
}

/// A failed real artifact lookup is one non-retryable consuming-request
/// terminal, not a backend attempt or a provider-process failure.
#[test]
fn missing_referenced_image_terminalizes_without_backend_retry_or_key_disclosure() {
    for operation in [
        tau_proto::PromptOperation::Inference,
        tau_proto::PromptOperation::StandaloneCompaction,
    ] {
        resolver_fixture::with_transport(|handle, waker| {
            let mut state = State::default();
            let (tx, rx) = mpsc::channel();
            let resolver = state.client(tx, waker);
            let worker = thread::spawn(move || {
                let mut prompt = minimal_prompt();
                prompt.operation = operation;
                prompt.context = resolver_fixture::context();
                let original = prompt.clone();
                let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
                let mut bytes = Vec::new();
                let retry = handle_prompt_backend(
                    &prompt.agent_prompt_id,
                    &image_backend(),
                    &prompt,
                    &mut tau_proto::PeerOutputWriter::new(&mut bytes),
                    &mut RecordingRetrySleeper,
                    ChatGptPromptExecutionContext {
                        image_resolution: Some(&resolver),
                        debug_provider_requests: false,
                        runtime: &runtime,
                        prior_backend: None,
                        logical_attempt: tau_provider_codex::LogicalAttempt::new(3),
                        compact_route_unavailable: &|_| panic!("no model request"),
                    },
                    &mut |_| panic!("no quota request"),
                )
                .expect("terminal");
                assert!(
                    retry.is_none(),
                    "resolution failure cannot reach provider retry"
                );
                assert_eq!(
                    prompt, original,
                    "retained retry input remains reference-backed"
                );
                let frames = decode_frames(&bytes);
                assert_eq!(frames.len(), 1);
                let HarnessInputMessage::Emit(emit) = &frames[0] else {
                    panic!("terminal")
                };
                let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
                    panic!("terminal")
                };
                assert_eq!(
                    finished.failure_kind,
                    Some(tau_proto::ProviderFailureKind::RequestRejected)
                );
                assert_eq!(finished.provider_attempt.get(), 3);
                assert!(
                    !format!("{finished:?}").contains(
                        resolver_fixture::descriptor(&resolver_fixture::png())
                            .key
                            .as_str()
                    )
                );
            });
            resolver_fixture::serve(&mut state, &rx, worker, handle, |_| {
                Err(tau_proto::ArtifactError::Unavailable)
            });
        });
    }
}

/// A text-only route preserves the existing explicit omission without reading
/// prepared bytes or disclosing their fetchable descriptor upstream.
#[test]
fn text_only_route_omits_references_without_artifact_fetch() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    resolver_fixture::with_transport(|_, waker| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let key = resolver_fixture::descriptor(&resolver_fixture::png()).key;
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("deadline");
            let mut reader = BufReader::new(&mut stream);
            let mut length = None;
            let mut header_bytes = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                header_bytes += line.len();
                assert!(header_bytes < 16_384);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().expect("length"));
                }
            }
            let length = length.expect("body length");
            assert!(length < 65_536);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
            let wire = body.to_string();
            assert!(wire.contains("image omitted"));
            assert!(!wire.contains(key.as_str()));
            assert!(!wire.contains("data:image/"));
            let response = "data: {\"choices\":[{\"delta\":{\"content\":\"text-only success\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).expect("response");
        });
        let state = State::default();
        let (tx, rx) = mpsc::channel();
        let resolver = state.client(tx, waker);
        let provider = ChatCompletionsProvider {
            base_url: format!("http://{address}"),
            models: vec![
                serde_json::from_value(serde_json::json!({"id": "text-only"})).expect("model"),
            ],
            ..Default::default()
        };
        let backend = PromptBackend::ChatCompletions {
            provider: Arc::new(provider),
            model_index: 0,
        };
        let mut prompt = minimal_prompt();
        prompt.context = resolver_fixture::context();
        let runtime = CodexRuntime::new(Arc::new(test_network_policy()));
        let mut bytes = Vec::new();
        let retry = handle_prompt_backend(
            &prompt.agent_prompt_id,
            &backend,
            &prompt,
            &mut tau_proto::PeerOutputWriter::new(&mut bytes),
            &mut RecordingRetrySleeper,
            ChatGptPromptExecutionContext {
                image_resolution: Some(&resolver),
                debug_provider_requests: false,
                runtime: &runtime,
                prior_backend: None,
                logical_attempt: tau_provider_codex::LogicalAttempt::new(1),
                compact_route_unavailable: &|_| {},
            },
            &mut |_| {},
        )
        .expect("text-only request");
        assert!(retry.is_none());
        assert!(
            rx.try_recv().is_err(),
            "unsupported route must not fetch artifacts"
        );
        assert!(decode_frames(&bytes).iter().any(|frame| matches!(frame,
            HarnessInputMessage::Emit(emit) if matches!(emit.event.as_ref(),
                Event::ProviderResponseFinishedReported(finished)
                    if finished.stop_reason == tau_proto::ProviderStopReason::EndTurn))));
        server.join().expect("server");
    });
}
