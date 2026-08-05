use std::io::Write as _;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic as path_std_sync_atomic;
use std::{
    collections as path_std_collections, io as path_std_io, net as path_std_net,
    sync as path_std_sync,
};

mod scripted_tcp_server;

use scripted_tcp_server::ScriptedTcpServer;

use super::*;

/// Provider usage preserves an explicitly reported all-zero record while
/// retaining complete field absence as unavailable.
#[test]
fn stream_usage_distinguishes_absent_from_zero() {
    let mut state = StreamState::new();
    assert_eq!(state.usage(), None);

    state.input_tokens = Some(0);
    state.cached_tokens = Some(0);
    state.output_tokens = Some(0);
    let usage = state.usage().expect("reported zero usage");
    assert_eq!(usage.prompt_sent_tokens, 0);
    assert_eq!(usage.prompt_cached_tokens, 0);
    assert_eq!(usage.response_received_tokens, 0);
}

/// Cache counters are ignored unless the configured route explicitly declares
/// the matching OpenAI-compatible usage schema.
#[test]
fn cache_usage_requires_explicit_route_capability() {
    let wire_usage = serde_json::json!({
        "prompt_tokens": 100,
        "completion_tokens": 5,
        "prompt_tokens_details": {
            "cached_tokens": 80,
            "cache_write_tokens": 10
        }
    });
    let mut disabled = StreamState::new();
    capture_usage(&mut disabled, &wire_usage);
    let disabled = disabled.usage().expect("ordinary usage remains available");
    assert_eq!(disabled.prompt_cached_tokens, 0);
    assert_eq!(disabled.cache, None);

    let mut enabled = StreamState::new_with_cache_usage(CacheUsageCompat::OpenAi);
    capture_usage(&mut enabled, &wire_usage);
    let enabled = enabled.usage().expect("explicit cache usage");
    let cache = enabled.cache.expect("normalized cache observations");
    assert_eq!(cache.read_tokens, Some(80));
    assert_eq!(cache.write_tokens, Some(10));
    assert_eq!(cache.avoided_prefill_tokens, Some(80));
}

/// An explicitly reported zero cache read remains distinguishable from a
/// missing cache observation on a capable route.
#[test]
fn explicit_zero_cache_usage_remains_present() {
    let mut state = StreamState::new_with_cache_usage(CacheUsageCompat::OpenAi);
    capture_usage(
        &mut state,
        &serde_json::json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": 0}
        }),
    );

    let cache = state
        .usage()
        .expect("explicit zero usage")
        .cache
        .expect("explicit zero cache observation");
    assert_eq!(cache.read_tokens, Some(0));

    let mut absent = StreamState::new_with_cache_usage(CacheUsageCompat::OpenAi);
    capture_usage(
        &mut absent,
        &serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 1
        }),
    );
    assert_eq!(
        absent.usage().expect("ordinary usage").cache,
        None,
        "a capable route must not invent missing cache observations"
    );
}

/// DeepSeek hit and miss fields use their declared schema and contradictory
/// counters clamp deterministically in read-then-write-then-miss order.
#[test]
fn deepseek_cache_usage_normalizes_contradictory_counters() {
    let mut state = StreamState::new_with_cache_usage(CacheUsageCompat::DeepSeek);
    capture_usage(
        &mut state,
        &serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 0,
            "prompt_cache_hit_tokens": 90,
            "prompt_cache_miss_tokens": 90
        }),
    );

    let usage = state.usage().expect("DeepSeek usage");
    let cache = usage.cache.expect("DeepSeek cache observations");
    assert_eq!(cache.read_tokens, Some(90));
    assert_eq!(cache.miss_tokens, Some(10));
    assert_eq!(
        cache.expiry_confidence,
        Some(tau_proto::ProviderCacheExpiryConfidence::Probabilistic)
    );
    assert_eq!(cache.hit_ratio_millionths(), Some(900_000));
}

/// Ensures historical XML-escaped and current exact-close web results remain
/// byte-for-byte intact on the Chat Completions native tool-result path.
#[test]
fn web_content_envelope_is_preserved_in_chat_tool_result() {
    for envelope in [
        "<tau_web_content adapter=\"exa\" operation=\"search\" content_trust=\"external\">Title: &lt;claim&gt;</tau_web_content>",
        "<tau_web_content adapter=\"exa\" operation=\"search\" content_trust=\"external\">Title: <claim> & &lt;/tau_web_content&gt;</tau_web_content>",
    ] {
        let output =
            tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(envelope.to_owned()));
        assert_eq!(
            tool_result_text(tau_proto::ToolResultStatus::Success, &output),
            envelope
        );
    }
}

/// Ensures shared outbound categories retain their intended scheduler cadence
/// at the Chat Completions adapter boundary.
#[test]
fn outbound_categories_map_to_retry_classes() {
    use tau_provider::OutboundErrorKind as Kind;

    for (kind, expected) in [
        (Kind::InvalidConfiguration, RetryClass::Auth),
        (Kind::ProxyAuthentication, RetryClass::Auth),
        (Kind::Transport, RetryClass::Transport),
        (Kind::Deadline, RetryClass::Transport),
        (Kind::Protocol, RetryClass::Transport),
    ] {
        assert_eq!(outbound_retry_class(kind), expected, "{kind:?}");
    }
}

fn provider() -> TestProvider {
    TestProvider {
        base_url: "http://localhost:1234/v1".to_owned(),
        api_key: String::new(),
        models: vec![AttemptModel {
            id: ModelName::new("test-model"),
        }],
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        compat: AttemptCompat {
            stream_options: true,
            parallel_tool_calls: true,
            prompt_cache_key: true,
            reasoning_effort: true,
            max_completion_tokens: true,
            cache_usage: CacheUsageCompat::None,
        },
    }
}

struct TestProvider {
    base_url: String,
    api_key: String,
    models: Vec<AttemptModel>,
    max_output_tokens: u32,
    extra_body: BTreeMap<String, serde_json::Value>,
    compat: AttemptCompat,
}

fn resolved_provider(provider: &TestProvider) -> AttemptConfig {
    AttemptConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        max_output_tokens: provider.max_output_tokens,
        local_summary_compaction: None,
        extra_body: provider.extra_body.clone(),
        compat: provider.compat,
    }
}
/// Ensures streamed Chat Completions tool-call arguments contribute only
/// content-free bytes to provider-owned response stats.
#[test]
fn stream_state_reports_tool_argument_response_bytes() {
    let mut state = StreamState::new();

    state
        .append_tool_arguments_delta(0, "{\"cmd\":")
        .expect("first argument delta");
    state
        .append_tool_arguments_delta(0, "\"ls\"}")
        .expect("second argument delta");

    assert_eq!(
        state.response_bytes_received(),
        r#"{"cmd":"ls"}"#.len() as u64,
    );
}

/// Ensures transport progress moves as soon as response bytes arrive, even when
/// the provider has not yet sent a complete SSE line that can be parsed
/// semantically.
#[test]
fn chat_stream_body_counts_transport_bytes_before_complete_sse_line() {
    let bytes = b"data: {\"choices\"";
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let mut observed = Vec::new();

    read_chat_stream_body(
        path_std_io::Cursor::new(bytes),
        &mut state,
        &mut raw_events,
        &mut |state| observed.push(state.response_bytes_received()),
        &mut || false,
    )
    .expect("partial stream read");

    assert_eq!(observed, vec![bytes.len() as u64]);
    assert!(raw_events.is_empty());
}

struct DoneThenPanicReader {
    sent_done: bool,
}

impl std::io::Read for DoneThenPanicReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent_done {
            panic!("reader must not be polled again after data: [DONE]");
        }
        self.sent_done = true;
        let bytes = b"data: [DONE]\n\n";
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }
}

/// Ensures `[DONE]` terminates the parser without waiting for EOF from a
/// provider that leaves the HTTP body open.
#[test]
fn chat_stream_body_stops_after_done_without_waiting_for_eof() {
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let mut updates = 0;

    read_chat_stream_body(
        DoneThenPanicReader { sent_done: false },
        &mut state,
        &mut raw_events,
        &mut |_| updates += 1,
        &mut || false,
    )
    .expect("done stream read");

    assert_eq!(updates, 1);
    assert_eq!(
        state.response_bytes_received(),
        b"data: [DONE]\n\n".len() as u64
    );
    assert!(raw_events.is_empty());
}

/// Ensures active Chat Completions stream cancellation closes the attempt
/// before any additional provider bytes can become durable output.
#[test]
fn chat_stream_body_observes_prompt_cancellation() {
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let error = read_chat_stream_body(
        path_std_io::Cursor::new(b"data: never read\n\n"),
        &mut state,
        &mut raw_events,
        &mut |_| {},
        &mut || true,
    )
    .expect_err("canceled stream must stop");
    assert!(matches!(error, LlmError::Canceled));
    assert!(state.output_items.is_empty());
}

/// Runs a real reqwest attempt against a local peer that deliberately stalls
/// either before response headers or after successful SSE headers.
fn assert_reqwest_stall_is_canceled(after_headers: bool) {
    let listener =
        path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind cancellation server");
    let address = listener.local_addr().expect("cancellation server address");
    let (accepted_tx, accepted_rx) = path_std_sync::mpsc::sync_channel(1);
    let (dropped_tx, dropped_rx) = path_std_sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept cancellation request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let mut request = [0_u8; 8192];
        let read = stream.read(&mut request).expect("read request bytes");
        assert!(read > 0, "client must send a request before cancellation");
        if after_headers {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                )
                .expect("write stalled response headers");
            stream.flush().expect("flush stalled response headers");
        }
        accepted_tx.send(()).expect("report accepted request");
        loop {
            match stream.read(&mut request) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("client connection was not dropped promptly: {error}"),
            }
        }
        dropped_tx.send(()).expect("report dropped connection");
    });

    let canceled = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let attempt_canceled = path_std_sync::Arc::clone(&canceled);
    let (result_tx, result_rx) = path_std_sync::mpsc::sync_channel(1);
    let attempt = std::thread::spawn(move || {
        let mut configured = provider();
        configured.base_url = format!("http://{address}/v1");
        let model = configured.models[0].clone();
        let prompt = prompt();
        let resolved = resolved_provider(&configured);
        let outcome = run_attempt(
            &prompt,
            &resolved,
            &model,
            false,
            &mut |_| {},
            &mut || attempt_canceled.load(path_std_sync_atomic::Ordering::SeqCst),
            &tau_provider::OutboundNetworkPolicy::from_environment(
                path_std_collections::BTreeMap::new(),
                None,
            ),
        );
        result_tx.send(outcome).expect("report attempt outcome");
    });
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reqwest request did not reach local peer");
    canceled.store(true, path_std_sync_atomic::Ordering::SeqCst);
    assert!(matches!(
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("canceled reqwest attempt stayed blocked"),
        AttemptOutcome::Canceled { .. }
    ));
    dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("canceled reqwest future retained its TCP connection");
    attempt.join().expect("attempt thread");
    server.join().expect("server thread");
}

/// Cancellation while awaiting HTTP headers drops the reqwest future/socket.
#[test]
fn reqwest_awaiting_headers_is_prompt_cancelable() {
    assert_reqwest_stall_is_canceled(false);
}

/// Cancellation while a successful SSE body is stalled drops future/socket.
#[test]
fn reqwest_stalled_success_body_is_prompt_cancelable() {
    assert_reqwest_stall_is_canceled(true);
}

/// Ensures transport failures crossing the Chat Completions attempt facade
/// expose only closed retry facts, never target/proxy credentials or endpoints.
#[test]
fn attempt_transport_failure_redacts_backend_canaries() {
    let proxy = ScriptedTcpServer::spawn(|socket| {
        drop(socket);
    });
    let address = proxy.address();
    let mut configured = provider();
    configured.base_url = "http://target-backend-canary.invalid/v1".to_owned();
    configured.api_key = "bearer-backend-canary".to_owned();
    let model = configured.models[0].clone();
    let prompt = prompt();
    let resolved = resolved_provider(&configured);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::from([(
            "http_proxy".to_owned(),
            format!("http://proxy-user-canary:proxy-pass-canary@{address}"),
        )]),
        None,
    );
    let outcome = run_attempt(
        &prompt,
        &resolved,
        &model,
        false,
        &mut |_| {},
        &mut || false,
        &network,
    );
    proxy.finish();
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
    let projection = format!("{outcome:?}");
    for canary in [
        "target-backend-canary",
        "bearer-backend-canary",
        "proxy-user-canary",
        "proxy-pass-canary",
        &address.to_string(),
    ] {
        assert!(
            !projection.contains(canary),
            "leaked {canary}: {projection}"
        );
    }
}

/// A finite attempt reports exactly one dispatch observation before the backend
/// can accept its first request bytes.
#[test]
fn attempt_dispatch_precedes_backend_send_and_occurs_once() {
    let dispatched = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let server_observation = path_std_sync::Arc::clone(&dispatched);
    let server = ScriptedTcpServer::spawn(move |mut socket| {
        assert!(server_observation.load(std::sync::atomic::Ordering::SeqCst));
        let mut request = [0_u8; 4096];
        let bytes_read = path_std_io::Read::read(&mut socket, &mut request).expect("read request");
        assert!(0 < bytes_read);
    });
    let mut configured = provider();
    configured.base_url = format!("http://{}/v1", server.address());
    let model = configured.models[0].clone();
    let resolved = resolved_provider(&configured);
    let mut dispatch_count = 0;
    let outcome = run_attempt(
        &prompt(),
        &resolved,
        &model,
        false,
        &mut |update| {
            if matches!(update, AttemptUpdate::Dispatched(_)) {
                dispatch_count += 1;
                dispatched.store(true, path_std_sync_atomic::Ordering::SeqCst);
            }
        },
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(
            path_std_collections::BTreeMap::new(),
            None,
        ),
    );
    server.finish();
    assert_eq!(dispatch_count, 1);
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
}

/// First-output timing uses accepted semantic state rather than replay-safety
/// progress, raw delimiters, ids, or empty structures.
#[test]
fn timed_semantic_output_predicate_matches_chat_contract() {
    let mut state = StreamState::new();
    state.record_transport_response_bytes(10);
    state.pending_content.push_str("<think>");
    state.tool_call_at_mut(0).id.push_str("call-only");
    assert!(!state.has_timed_semantic_output());

    state.tool_call_at_mut(0).name.push_str("lookup");
    assert!(state.has_timed_semantic_output());

    let mut arguments = StreamState::new();
    arguments
        .append_tool_arguments_delta(0, "{\"q\":")
        .expect("accepted arguments");
    assert!(arguments.has_timed_semantic_output());

    let mut text = StreamState::new();
    text.append_assistant_text_delta("hello")
        .expect("accepted assistant text");
    assert!(text.has_timed_semantic_output());

    let mut reasoning = StreamState::new();
    reasoning
        .append_reasoning_delta("why")
        .expect("accepted reasoning text");
    assert!(reasoning.has_timed_semantic_output());
}

/// Ensures a malicious provider cannot grow the pending SSE-line buffer
/// without bound by streaming bytes forever without a newline.
#[test]
fn chat_stream_body_rejects_oversized_partial_line() {
    let bytes = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let error = read_chat_stream_body(
        path_std_io::Cursor::new(bytes),
        &mut state,
        &mut raw_events,
        &mut |_| {},
        &mut || false,
    )
    .expect_err("oversized SSE line must be bounded");
    assert!(matches!(
        error,
        LlmError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData
    ));
}

/// Ensures debug event retention is bounded even when an upstream emits many
/// syntactically valid SSE events before completion.
#[test]
fn chat_stream_body_bounds_debug_event_retention() {
    let mut bytes = Vec::new();
    for _ in 0..(MAX_DEBUG_EVENTS + 10) {
        bytes.extend_from_slice(b"data: {}\n");
    }
    bytes.extend_from_slice(b"data: [DONE]\n");
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    read_chat_stream_body(
        path_std_io::Cursor::new(bytes),
        &mut state,
        &mut raw_events,
        &mut |_| {},
        &mut || false,
    )
    .expect("bounded event stream");
    assert_eq!(raw_events.len(), MAX_DEBUG_EVENTS);
}

/// Ensures SSE comments and blank heartbeat lines do not reset the semantic
/// idle watchdog while actual `data:` provider events do.
#[test]
fn stream_idle_progress_requires_data_event() {
    assert!(!sse_lines_have_provider_event(&[
        ": keepalive".to_owned(),
        String::new(),
    ]));
    assert!(sse_lines_have_provider_event(&[
        ": keepalive".to_owned(),
        "data: {}".to_owned(),
    ]));
}
fn prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-test"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
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
        operation: tau_proto::PromptOperation::Inference,
    }
}

/// Ensures real request, successful-response, and HTTP-error producers submit
/// the expected metadata through the shared compressed-capture job boundary.
#[test]
fn debug_capture_producers_submit_typed_http_sse_jobs() {
    let prompt = prompt();
    let provider = provider();
    let model = &provider.models[0];
    let request = build_request(&resolved_provider(&provider), model, &prompt);
    let state = StreamState::new();
    let raw_event = serde_json::json!({"done": true});
    let mut submitted = Vec::new();
    let mut record = |capture: tau_provider::debug_capture_writer::ProviderDebugCapture| {
        assert_eq!(capture.session_id(), &prompt.session_id);
        assert_eq!(capture.agent_prompt_id(), &prompt.agent_prompt_id);
        submitted.push((
            capture.class(),
            serde_json::from_slice::<serde_json::Value>(capture.json()).expect("capture JSON"),
        ));
    };

    maybe_debug_submit_provider_request_with(&prompt, model, true, &request, &mut record);
    maybe_debug_submit_provider_response_with(
        &prompt,
        model,
        true,
        &state,
        std::slice::from_ref(&raw_event),
        &mut record,
    );
    maybe_debug_submit_provider_http_error_with(
        &prompt,
        model,
        true,
        429,
        "bounded error",
        &mut record,
    );

    assert_eq!(submitted.len(), 3);
    assert_eq!(
        submitted[0].0,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseRequest
    );
    assert_eq!(submitted[0].1["body"]["model"], "test-model");
    assert_eq!(
        submitted[1].0,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
    );
    assert_eq!(submitted[1].1["raw_events"][0], raw_event);
    assert_eq!(
        submitted[2].0,
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
    );
    assert_eq!(submitted[2].1["http_status"], 429);
    assert_eq!(submitted[2].1["body"], "bounded error");
}

/// Chat Completions must reject Custom tools visibly rather than silently
/// dropping a definition that escaped harness filtering.
#[test]
fn chat_request_rejects_custom_tool_definition() {
    let mut created = prompt();
    created.tools.push(tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("custom_text"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Custom,
        parameters: None,
        format: None,
    });
    let result = try_build_request(
        &resolved_provider(&provider()),
        &provider().models[0],
        &created,
    );
    assert!(matches!(
        result,
        Err(LlmError::UnsupportedToolType(tau_proto::ToolType::Custom))
    ));
}

/// Ensures summary compaction uses a dedicated static, no-tools request rather
/// than silently reusing ordinary inference request construction.
#[test]
fn local_summary_compaction_builds_dedicated_bounded_request() {
    let mut created = prompt();
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    created.system_prompt = "ordinary agent authority must not leak".to_owned();
    created.tools.push(tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("dangerous"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    });
    created
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    call_id: tau_proto::ToolCallId::new("call-image"),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                        "image result".to_owned(),
                    )),
                    provider_content: vec![tau_proto::ToolResultContentPart::Image(
                        tau_proto::ImageContent {
                            media_type: tau_proto::ImageMediaType::Png,
                            data: std::sync::Arc::from([11_u8, 22, 33]),
                            width: 17,
                            height: 19,
                            detail: tau_proto::ImageDetail::High,
                        },
                    )],
                }],
            },
        ));
    let mut config = resolved_provider(&provider());
    config.local_summary_compaction = Some(LocalSummaryCompactionConfig {
        context_window_tokens: NonZeroU64::new(8192).expect("positive"),
        max_input_bytes: NonZeroU64::new(4096).expect("positive"),
        max_output_tokens: NonZeroU32::new(321).expect("positive"),
        max_output_bytes: NonZeroU64::new(2048).expect("positive"),
    });

    let request = try_build_request(&config, &provider().models[0], &created)
        .expect("enabled summary request");

    assert!(request.tools.is_empty());
    assert_eq!(request.tool_choice, Some("none"));
    assert_eq!(request.max_completion_tokens, Some(321));
    assert_eq!(request.prompt_cache_key, None);
    assert_eq!(request.reasoning_effort, None);
    let messages = serde_json::to_value(&request.messages).expect("messages");
    assert!(
        !messages
            .to_string()
            .contains("ordinary agent authority must not leak"),
        "ordinary system prompt must not become compactor authority"
    );
    let input = messages[1]["content"].as_str().expect("compactor input");
    assert!(input.contains("\"tau_compaction_transcript_version\":1"));
    assert!(input.contains("canonical image bytes omitted intentionally"));
    assert!(input.contains("\"media_type\":\"png\""));
    assert!(input.contains("\"width\":17"));
    assert!(input.contains("\"height\":19"));
    assert!(input.contains("\"detail\":\"high\""));
    assert!(!input.contains("11"));
    assert!(!input.contains("22"));
    assert!(!input.contains("33"));
}

/// Ensures an oversized canonical transcript fails before request dispatch
/// instead of shrinking the already selected compaction input.
#[test]
fn local_summary_compaction_rejects_oversized_input_without_truncation() {
    let mut created = prompt();
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut config = resolved_provider(&provider());
    config.local_summary_compaction = Some(LocalSummaryCompactionConfig {
        context_window_tokens: NonZeroU64::new(8192).expect("positive"),
        max_input_bytes: NonZeroU64::new(1).expect("positive"),
        max_output_tokens: NonZeroU32::new(1).expect("positive"),
        max_output_bytes: NonZeroU64::new(1).expect("positive"),
    });

    assert!(matches!(
        try_build_request(&config, &provider().models[0], &created),
        Err(LlmError::InvalidCompaction(_))
    ));
}

/// Ensures even explicitly enabled durable provider diagnostics never persist a
/// full standalone compactor input.
#[test]
fn local_summary_compaction_suppresses_request_debug_capture() {
    let mut created = prompt();
    assert!(debug_capture_enabled_for_prompt(&created, true));
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    assert!(!debug_capture_enabled_for_prompt(&created, true));
    assert!(!debug_capture_enabled_for_prompt(&created, false));
}

/// Ensures conservative one-byte-per-token accounting accepts the exact
/// declared boundary and rejects one token less before dispatch.
#[test]
fn local_summary_compaction_enforces_complete_context_budget_boundary() {
    let mut created = prompt();
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut config = resolved_provider(&provider());
    let overhead = LOCAL_SUMMARY_COMPACTION_REQUEST_OVERHEAD_TOKENS;
    config.local_summary_compaction = LocalSummaryCompactionConfig::new(
        NonZeroU64::new(4096 + 1 + overhead).expect("positive"),
        4096 + 1 + overhead,
        NonZeroU64::new(4096).expect("positive"),
        NonZeroU32::new(1).expect("positive"),
        NonZeroU64::new(1).expect("positive"),
    );
    assert!(try_build_request(&config, &provider().models[0], &created).is_ok());

    assert!(
        LocalSummaryCompactionConfig::new(
            NonZeroU64::new(4096 + overhead).expect("positive"),
            4096 + overhead,
            NonZeroU64::new(4096).expect("positive"),
            NonZeroU32::new(1).expect("positive"),
            NonZeroU64::new(1).expect("positive"),
        )
        .is_none()
    );
}

/// Ensures request emission depends only on wire compatibility and tool
/// presence: both booleans are independently represented in the 2x2 matrix.
#[test]
fn parallel_tool_request_field_follows_compatibility_and_tool_presence() {
    for (compatibility, has_tools, expected) in [
        (false, false, None),
        (false, true, None),
        (true, false, None),
        (true, true, Some(true)),
    ] {
        let mut provider = provider();
        provider.compat.parallel_tool_calls = compatibility;
        let mut created = prompt();
        if has_tools {
            created.tools.push(tau_proto::ToolDefinition {
                name: tau_proto::ToolName::new("lookup"),
                model_visible_name: None,
                description: Some("lookup".to_owned()),
                parameters: Some(serde_json::json!({"type": "object"})),
                format: None,
                tool_type: ToolType::Function,
            });
        }
        let request = build_request(&resolved_provider(&provider), &provider.models[0], &created);
        assert_eq!(
            request.parallel_tool_calls, expected,
            "compatibility={compatibility}, has_tools={has_tools}"
        );
    }
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
fn extra_body_rejects_every_reserved_request_member() {
    // A flattened duplicate would make request meaning serializer-dependent.
    // Reject reserved keys before any provider dispatch.
    for field in [
        "model",
        "messages",
        "stream",
        "stream_options",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "prompt_cache_key",
        "reasoning_effort",
        "max_tokens",
        "max_completion_tokens",
    ] {
        let mut provider = provider();
        provider
            .extra_body
            .insert(field.to_owned(), serde_json::json!("collision"));
        let Err(error) = try_build_request(
            &resolved_provider(&provider),
            &provider.models[0],
            &prompt(),
        ) else {
            panic!("reserved extra_body member {field} must be rejected");
        };
        assert!(matches!(error, LlmError::ExtraBodyCollision(actual) if actual == field));
    }
}

/// Ensures genuinely provider-specific request members still pass through
/// unchanged after reserved-member validation.
#[test]
fn extra_body_preserves_non_conflicting_member() {
    let mut provider = provider();
    provider.extra_body.insert(
        "chat_template_kwargs".to_owned(),
        serde_json::json!({"enable_thinking": true}),
    );
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    assert_eq!(
        request.extra_body["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": true})
    );
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

/// Deterministic request statuses are terminal, but recursive echoes and prose
/// never manufacture the more specific context-window category.
#[test]
fn deterministic_request_status_is_terminal_without_trusting_echoes() {
    for body in [
        r#"{"error":{"code":"unsupported_parameter"}}"#,
        r#"{"error":{"message":"temporary upstream failure"},"echo":{"code":"unsupported_parameter"}}"#,
        r#"{"error":{"message":"temporary (type=content_policy_violation)"}}"#,
    ] {
        assert_eq!(retry_decision_for_http_error(400, body, None), None);
        assert!(
            !canonical_error_identifiers(body)
                .iter()
                .any(|identifier| identifier == "context_length_exceeded")
        );
    }
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
fn accepted_content_preserves_progress_when_later_tool_delta_repeats() {
    let mut state = StreamState::new();
    let mut observed_progress = SemanticProgress::None;
    let result = apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "ok",
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "name": "shell",
                            "arguments": "_clone".repeat(180)
                        }
                    }]
                }
            }]
        }),
        &mut |state| observed_progress = state.semantic_progress,
    );

    assert!(matches!(result, Err(LlmError::RepetitionDetected(_))));
    assert_eq!(observed_progress, SemanticProgress::Parsed);
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
/// OpenAI and OpenRouter canonical context errors are typed and terminal,
/// independent of an outer scheduler's retry budget.
#[test]
fn canonical_context_error_bypasses_retry_scheduler() {
    let error = LlmError::HttpStatus(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded"}}"#.to_owned(),
    );
    assert_eq!(error.retry_decision(), None);
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
}

/// Ensures bounded raw HTTP bodies remain private classification/debug input
/// and never cross the typed attempt facade into a public final error.
#[test]
fn terminal_http_failure_redacts_provider_body() {
    let secret = "SENTINEL_PROVIDER_BODY_SECRET";
    let error = LlmError::HttpStatus(400, format!(r#"{{"error":{{"message":"{secret}"}}}}"#));
    let AttemptOutcome::Terminal(failure) = finish_attempt(Err(error), SemanticProgress::None)
    else {
        panic!("deterministic HTTP 400 must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert!(!failure.message.contains(secret));
    assert_eq!(failure.message, "LLM error: provider returned HTTP 400");
}

/// Retry ownership remains unchanged for transient provider throttling.
#[test]
fn canonical_rate_limit_remains_retryable() {
    let decision =
        retry_decision_for_http_error(429, r#"{"error":{"code":"rate_limit_exceeded"}}"#, None)
            .expect("rate limit remains retryable");
    assert_eq!(decision.class, RetryClass::Throttle);
}

/// Status-only terminalization yields to canonical transient identifiers, but
/// not to untrusted echoed fields.
#[test]
fn deterministic_status_transient_override_uses_only_error_envelope() {
    assert!(
        retry_decision_for_http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","code":"rate_limit_exceeded"}}"#,
            None,
        )
        .is_some()
    );
    let echoed = r#"{"error":{"message":"rejected"},"echo":{"code":"rate_limit_exceeded"}}"#;
    assert_eq!(retry_decision_for_http_error(400, echoed, None), None);
    assert_eq!(
        http_failure_kind(400, echoed),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
}

/// Ensures streamed provider failures remain typed backend failures and can
/// never be converted into assistant transcript text.
#[test]
fn streamed_context_error_is_typed_terminal_without_assistant_output() {
    let mut state = StreamState::new();
    let error = apply_event(
        &mut state,
        &serde_json::json!({
            "error": {
                "code": 400,
                "metadata": {"error_type": "context_length_exceeded"},
                "message": "untrusted provider prose"
            }
        }),
        &mut |_| {},
    )
    .expect_err("stream error must stop parsing");

    assert!(state.output_items().is_empty());
    assert_eq!(error.to_string(), "provider returned a streamed error");
    let AttemptOutcome::Terminal(failure) = finish_attempt(Err(error), SemanticProgress::None)
    else {
        panic!("context stream error must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
}

/// Ensures a typed transient stream code retains scheduler classification while
/// provider-authored prose remains outside the structured outcome.
#[test]
fn streamed_rate_limit_error_is_typed_retryable() {
    let mut state = StreamState::new();
    let error = apply_event(
        &mut state,
        &serde_json::json!({
            "error": {
                "code": 429,
                "metadata": {"error_type": "provider_error"},
                "message": "secret-bearing arbitrary text"
            }
        }),
        &mut |_| {},
    )
    .expect_err("stream error must stop parsing");

    assert!(state.output_items().is_empty());
    assert!(!error.to_string().contains("secret-bearing"));
    let AttemptOutcome::Retryable { decision, progress } =
        finish_attempt(Err(error), SemanticProgress::Parsed)
    else {
        panic!("numeric streamed 429 must be retryable");
    };
    assert_eq!(decision.class, RetryClass::Throttle);
    assert_eq!(progress, SemanticProgress::Parsed);
}

#[test]
fn malformed_non_null_stream_error_fails_closed_before_choices() {
    for malformed in [
        serde_json::json!("fatal"),
        serde_json::json!(500),
        serde_json::json!(["fatal"]),
    ] {
        let mut state = StreamState::new();
        let result = apply_event(
            &mut state,
            &serde_json::json!({
                "error": malformed,
                "choices": [{"delta": {"content": "must-not-commit"}}]
            }),
            &mut |_| {},
        );
        assert!(matches!(result, Err(LlmError::StreamError(_))));
        assert!(state.output_items().is_empty());
    }
}

/// Ensures OpenRouter numeric overload codes retain their closed typed
/// scheduler category rather than degrading to an unknown retry.
#[test]
fn streamed_numeric_overload_is_typed_retryable() {
    for code in [500, 503, 599] {
        let error = classify_stream_error(
            serde_json::json!({"code": code, "message": "ignored"})
                .as_object()
                .expect("error object"),
        );
        assert_eq!(
            error.retry.expect("retryable overload").class,
            RetryClass::Overload
        );
    }
}

/// Ensures nullable optional error members on ordinary stream chunks are not
/// mistaken for provider failures.
#[test]
fn null_stream_error_is_ignored() {
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "error": null,
            "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
        }),
        &mut |_| {},
    )
    .expect("nullable error is not a failure");
    assert_eq!(state.output_items(), vec![assistant_text_item("ok")]);
}

/// Ensures incomplete tool slots retain their backend index so later metadata
/// cannot shift and duplicate already-sampled assistant text.
#[test]
fn progress_materialization_preserves_incomplete_tool_slot_indices() {
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call-1", "function": {"arguments": "{}"}
            }]}}]
        }),
        &mut |_| {},
    )
    .expect("partial tool");
    apply_event(
        &mut state,
        &serde_json::json!({"choices": [{"delta": {"content": "hello"}}]}),
        &mut |_| {},
    )
    .expect("interleaved text");
    let before = state.indexed_output_items();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].output_index, 1);

    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "function": {"name": "lookup"}
            }]}}]
        }),
        &mut |_| {},
    )
    .expect("late tool name");
    let after = state.indexed_output_items();
    assert_eq!(
        after
            .iter()
            .map(|output| output.output_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

/// Ensures semantic tracking at chunk cadence is constant-time and does not
/// materialize/reparse the growing output until the extension sampler asks.
#[test]
fn semantic_progress_checks_do_not_materialize_growing_output() {
    OUTPUT_MATERIALIZATIONS.with(|count| count.set(0));
    let mut state = StreamState::new();
    for index in 0..1_000 {
        state
            .append_tool_arguments_delta(0, &format!("{index},"))
            .expect("tool argument delta");
        let progress = AttemptProgress { state: &state };
        assert_eq!(progress.semantic_progress(), SemanticProgress::Parsed);
        let _ = progress.response_bytes_received();
    }
    assert_eq!(OUTPUT_MATERIALIZATIONS.with(std::cell::Cell::get), 0);
    let _ = AttemptProgress { state: &state }.materialize_output();
    assert_eq!(OUTPUT_MATERIALIZATIONS.with(std::cell::Cell::get), 1);
}

/// Ensures buffered think-tag prefixes and incomplete tool metadata count as
/// semantic progress even before either can render as a complete output item.
#[test]
fn partial_buffer_and_tool_metadata_are_semantic_progress() {
    let mut content = StreamState::new();
    append_content_delta(&mut content, "<thi").expect("partial think tag");
    assert_eq!(content.semantic_progress, SemanticProgress::Parsed);
    assert!(content.output_items().is_empty());

    let mut tool = StreamState::new();
    apply_event(
        &mut tool,
        &serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call-only", "function": {}
            }]}}]
        }),
        &mut |_| {},
    )
    .expect("partial tool metadata");
    assert_eq!(tool.semantic_progress, SemanticProgress::Parsed);
    assert!(tool.output_items().is_empty());
}
