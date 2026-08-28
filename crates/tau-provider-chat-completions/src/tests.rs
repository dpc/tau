use std::io::Write as _;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::{Arc, atomic as path_std_sync_atomic};
use std::{
    collections as path_std_collections, io as path_std_io, net as path_std_net,
    sync as path_std_sync,
};

mod scripted_tcp_server;

use scripted_tcp_server::ScriptedTcpServer;

use super::*;

#[derive(Clone, Copy)]
enum NumericStatusExpectation {
    Terminal,
    Retry(RetryClass),
}

fn numeric_status_cases() -> impl Iterator<Item = (u16, NumericStatusExpectation)> {
    (400..500)
        .map(|status| {
            let expected = match status {
                401 | 403 => NumericStatusExpectation::Retry(RetryClass::Auth),
                408 | 425 => NumericStatusExpectation::Retry(RetryClass::Transport),
                429 => NumericStatusExpectation::Retry(RetryClass::Throttle),
                _ => NumericStatusExpectation::Terminal,
            };
            (status, expected)
        })
        .chain([500, 503, 599].into_iter().map(|status| {
            (
                status,
                NumericStatusExpectation::Retry(RetryClass::Overload),
            )
        }))
}

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
/// the matching OpenAI-compatible usage schema. Accepted counters remain
/// ordinary-request observations with unknown expiry confidence, preventing
/// reads or writes from becoming TTL or renewal facts.
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
    assert_eq!(
        cache.refresh_reason,
        Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest)
    );
    assert_eq!(
        cache.expiry_confidence,
        Some(tau_proto::ProviderCacheExpiryConfidence::Unknown)
    );
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
            supports_image_tool_results: false,
        }],
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        compat: AttemptCompat {
            stream_options: true,
            parallel_tool_calls: true,
            prompt_cache: None,
            reasoning_effort: Some(ReasoningEffortWire::OpenAi),
            reasoning_replay: ReasoningReplay::ReasoningContent,
            single_initial_system_message: false,
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

/// Ensures truncated SSE data at EOF rejects the attempt after transport bytes
/// have been recorded, rather than treating an incomplete provider event as a
/// successful response.
#[test]
fn chat_stream_body_rejects_incomplete_data_at_eof() {
    let bytes = b"data: {\"choices\"";
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let mut observed = Vec::new();

    let error = read_chat_stream_body(
        path_std_io::Cursor::new(bytes),
        &mut state,
        &mut raw_events,
        &mut |state| observed.push(state.response_bytes_received()),
        &mut || false,
    )
    .expect_err("incomplete SSE data must reject the stream");

    assert_eq!(observed, vec![bytes.len() as u64]);
    assert!(raw_events.is_empty());
    assert!(matches!(error, LlmError::Json(_)));
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

/// Ensures one decoded HTTP chunk may contain more than one MiB of complete,
/// individually bounded SSE lines without tripping the residual-line bound.
#[test]
fn chat_stream_chunk_accepts_many_complete_sub_limit_lines() {
    let line = format!("data: {{\"padding\":\"{}\"}}\n", "x".repeat(1024));
    assert!(line.len() < MAX_SSE_LINE_BYTES);
    let line_count = MAX_SSE_LINE_BYTES / line.len() + 1;
    let bytes = line.repeat(line_count);
    assert!(MAX_SSE_LINE_BYTES < bytes.len());

    let mut pending = Vec::new();
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let outcome = process_stream_chunk(
        bytes.as_bytes(),
        &mut pending,
        &mut state,
        &mut raw_events,
        &mut |_| {},
    )
    .expect("complete sub-limit SSE lines must not exhaust the residual-line bound");

    assert!(!outcome.done);
    assert!(outcome.provider_event);
    assert!(pending.is_empty());
    assert_eq!(raw_events.len(), line_count);
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

/// Ensures malformed SSE data rejects the attempt after accepted output, so EOF
/// cannot complete a partial response while the retry owner retains that
/// progress.
#[test]
fn malformed_sse_data_after_delta_rejects_partial_stream() {
    let events = concat!(
        ": keepalive\n",
        "event: message\n",
        "id: ignored\n",
        "retry: 1000\n",
        "\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {malformed-json}",
    );
    let server = ScriptedTcpServer::spawn(move |mut socket| {
        let mut request = [0_u8; 16 * 1024];
        let _ = path_std_io::Read::read(&mut socket, &mut request).expect("read request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len()
        );
        socket
            .write_all(response.as_bytes())
            .expect("write streamed response");
    });
    let mut configured = provider();
    configured.base_url = format!("http://{}/v1", server.address());
    let model = configured.models[0].clone();
    let resolved = resolved_provider(&configured);
    let outcome = run_attempt(
        &prompt(),
        &resolved,
        &model,
        false,
        &mut |_| {},
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(
            path_std_collections::BTreeMap::new(),
            None,
        ),
    );
    server.finish();

    let AttemptOutcome::Retryable { decision, progress } = outcome else {
        panic!("malformed data must remain retryable");
    };
    assert_eq!(decision.class, RetryClass::Unknown);
    assert_eq!(progress, SemanticProgress::Parsed);
}

/// Ensures SSE comments and blank heartbeat lines do not reset the semantic
/// idle watchdog while actual `data:` provider events do.
#[test]
fn stream_idle_progress_requires_data_event() {
    for (bytes, expected_provider_event) in [
        (b": keepalive\n\n".as_slice(), false),
        (b": keepalive\ndata: {}\n".as_slice(), true),
    ] {
        let mut pending = Vec::new();
        let mut state = StreamState::new();
        let mut raw_events = Vec::new();
        let outcome = process_stream_chunk(
            bytes,
            &mut pending,
            &mut state,
            &mut raw_events,
            &mut |_| {},
        )
        .expect("SSE framing input must parse");
        assert!(!outcome.done);
        assert_eq!(outcome.provider_event, expected_provider_event);
    }
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

fn cache_prefix_prompt() -> tau_proto::AgentPromptCreated {
    let mut prompt = prompt();
    prompt.system_prompt = "stable system authority".to_owned();
    prompt.tools.push(tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("lookup"),
        model_visible_name: None,
        description: Some("Look up one value.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
            "additionalProperties": false,
        })),
        format: None,
    });
    prompt
}

/// Explicit cache controls must mark only the stable system-message text and
/// send the matching top-level OpenAI options, preventing implicit suffix
/// writes.
#[test]
fn explicit_prompt_cache_marks_the_system_prompt_boundary() {
    let mut provider = provider();
    provider.compat.prompt_cache = Some(PromptCache::ExplicitSystemPrompt);
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &cache_prefix_prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["prompt_cache_key"], "tau:agent-test");
    assert_eq!(
        json["prompt_cache_options"],
        serde_json::json!({"mode": "explicit", "ttl": "30m"})
    );
    assert!(json.get("prompt_cache_retention").is_none());
    assert_eq!(
        json["messages"][0]["content"],
        serde_json::json!([{
            "type": "text",
            "text": "stable system authority",
            "prompt_cache_breakpoint": {"mode": "explicit"},
        }])
    );
}

/// Explicit cache controls require a stable system prefix instead of moving the
/// breakpoint to volatile conversation content when no system prompt is
/// present.
#[test]
fn explicit_prompt_cache_rejects_an_empty_system_prompt() {
    let mut provider = provider();
    provider.compat.prompt_cache = Some(PromptCache::ExplicitSystemPrompt);
    let mut prompt = cache_prefix_prompt();
    prompt.system_prompt.clear();

    assert!(matches!(
        try_build_request(&resolved_provider(&provider), &provider.models[0], &prompt),
        Err(LlmError::PromptCacheSystemPromptRequired)
    ));
}

/// Legacy cache retention must retain the provider's automatic policy while
/// avoiding GPT-5.6 explicit-cache fields and synthetic content markers.
#[test]
fn legacy_prompt_cache_emits_only_the_legacy_top_level_fields() {
    let mut provider = provider();
    provider.compat.prompt_cache = Some(PromptCache::Legacy {
        retention: PromptCacheRetention::Hours24,
    });
    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &cache_prefix_prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");

    assert_eq!(json["prompt_cache_key"], "tau:agent-test");
    assert_eq!(json["prompt_cache_retention"], "24h");
    assert!(json.get("prompt_cache_options").is_none());
    assert_eq!(json["messages"][0]["content"], "stable system authority");
}

/// Ensures local correlation changes, appended history, and call suppression
/// retain the existing provider-visible lowering needed for prefix reuse.
#[test]
fn chat_request_keeps_stable_lowering_for_local_changes() {
    let provider = provider();
    let config = resolved_provider(&provider);
    let model = &provider.models[0];
    let created = cache_prefix_prompt();

    let stable = build_request(&config, model, &created);
    let stable_bytes = serde_json::to_vec(&stable).expect("serialize stable request");

    let mut irrelevant = created.clone();
    irrelevant.agent_prompt_id = "ap-next".parse().expect("prompt id");
    irrelevant.session_id = "session-next".parse().expect("session id");
    irrelevant.share_user_cache_key = true;
    assert_eq!(
        serde_json::to_vec(&build_request(&config, model, &irrelevant)).expect("serialize"),
        stable_bytes,
        "correlation and legacy cache-sharing fields must not perturb provider bytes"
    );

    let mut next_turn = created.clone();
    next_turn
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(tau_proto::MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text {
                        text: "newest volatile turn".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            },
        ));
    let next_request = build_request(&config, model, &next_turn);
    assert_eq!(
        next_request.messages[..stable.messages.len()],
        stable.messages,
        "new conversation content must follow the stable system/history prefix"
    );
    assert_eq!(next_request.tools, stable.tools);

    let mut disabled = created.clone();
    disabled.tool_choice = ToolChoice::None;
    let disabled_request = build_request(&config, model, &disabled);
    assert_eq!(disabled_request.tools, stable.tools);
    assert_eq!(disabled_request.tool_choice, Some("none"));
}

/// Ensures provider-visible model, authority, schema, and effort changes do
/// not accidentally reuse the request lowering for a different identity.
#[test]
fn chat_request_exposes_provider_visible_identity_changes() {
    let provider = provider();
    let config = resolved_provider(&provider);
    let model = &provider.models[0];
    let created = cache_prefix_prompt();
    let stable_bytes =
        serde_json::to_vec(&build_request(&config, model, &created)).expect("serialize");

    let mut changed_system = created.clone();
    changed_system.system_prompt.push('!');
    assert_ne!(
        serde_json::to_vec(&build_request(&config, model, &changed_system)).expect("serialize"),
        stable_bytes
    );

    let mut changed_tool = created.clone();
    changed_tool.tools[0].parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {"key": {"type": "integer"}},
    }));
    assert_ne!(
        serde_json::to_vec(&build_request(&config, model, &changed_tool)).expect("serialize"),
        stable_bytes
    );

    let mut changed_effort = created.clone();
    changed_effort.model_params.effort = tau_proto::Effort::High;
    assert_ne!(
        serde_json::to_vec(&build_request(&config, model, &changed_effort)).expect("serialize"),
        stable_bytes
    );

    let changed_model = AttemptModel {
        id: ModelName::new("other-model"),
        supports_image_tool_results: false,
    };
    assert_ne!(
        serde_json::to_vec(&build_request(&config, &changed_model, &created)).expect("serialize"),
        stable_bytes
    );
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

/// Summary compaction must preserve the exact ordinary lowered prefix,
/// including tools, images, raw arguments, and cache controls, then append one
/// instruction.
#[test]
fn local_summary_compaction_is_an_ordinary_cache_aligned_prefix() {
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
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    created
        .context
        .blocks
        .push(tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: tau_proto::ToolCallId::new("call-image"),
                    name: tau_proto::ToolName::new("dangerous"),
                    tool_type: ToolType::Function,
                    arguments: tau_proto::json_to_cbor(
                        &serde_json::from_str(raw_arguments).expect("valid raw arguments"),
                    ),
                    raw_arguments_json: Some(raw_arguments.to_owned()),
                    responses_envelope: None,
                })],
                usage: None,
            },
        ));
    created
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: tau_proto::ToolCallId::new("call-image"),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                        "image result".to_owned(),
                    )),
                    provider_content: vec![tau_proto::ToolResultContentPart::Image(
                        tau_proto::ImageContent {
                            media_type: tau_proto::ImageMediaType::Png,
                            data: Arc::from([11_u8, 22, 33]),
                            width: 17,
                            height: 19,
                            detail: tau_proto::ImageDetail::High,
                        },
                    )],
                }],
            },
        ));
    created
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            },
        ));
    let mut config = resolved_provider(&provider());
    config.compat.prompt_cache = Some(PromptCache::ExplicitSystemPrompt);
    config.local_summary_compaction = LocalSummaryCompactionConfig::new(
        NonZeroU64::new(8192).expect("positive"),
        8192,
        NonZeroU64::new(4096).expect("positive"),
        NonZeroU32::new(321).expect("positive"),
        NonZeroU64::new(2048).expect("positive"),
    );

    let mut model = provider().models[0].clone();
    model.supports_image_tool_results = true;
    let request = try_build_request(&config, &model, &created).expect("enabled summary request");
    let mut ordinary_prompt = created.clone();
    ordinary_prompt.operation = tau_proto::PromptOperation::Inference;
    ordinary_prompt.context.blocks.pop();
    let ordinary =
        try_build_request(&config, &model, &ordinary_prompt).expect("ordinary warmed request");

    assert_eq!(
        &request.messages[..ordinary.messages.len()],
        ordinary.messages.as_slice()
    );
    assert_eq!(request.tools, ordinary.tools);
    assert_eq!(request.tool_choice, ordinary.tool_choice);
    assert_eq!(request.parallel_tool_calls, ordinary.parallel_tool_calls);
    assert_eq!(request.prompt_cache_key, ordinary.prompt_cache_key);
    assert_eq!(
        request.prompt_cache_retention,
        ordinary.prompt_cache_retention
    );
    assert_eq!(
        serde_json::to_value(&request.prompt_cache_options).expect("cache options"),
        serde_json::to_value(&ordinary.prompt_cache_options).expect("cache options")
    );
    assert_eq!(request.reasoning_effort, ordinary.reasoning_effort);
    assert_eq!(request.max_completion_tokens, Some(321));
    assert_eq!(
        ordinary.messages[ordinary.messages.len() - 2]["tool_calls"][0]["function"]["arguments"],
        raw_arguments
    );
    assert_eq!(
        request.messages[ordinary.messages.len() - 2]["tool_calls"][0]["function"]["arguments"],
        raw_arguments
    );
    assert_eq!(
        request.messages.last(),
        Some(&serde_json::json!({
            "role": "user",
            "content": tau_provider::local_summary_compaction::REQUEST,
        }))
    );
    let wire = serde_json::to_string(&request).expect("summary request");
    assert!(wire.contains("ordinary agent authority must not leak"));
    assert!(wire.contains("data:image/png;base64,CxYh"));
}

/// Historical-prefix admission uses only exact bytes: equality is accepted and
/// the same prefix is rejected when the independent byte cap decreases by one.
#[test]
fn local_summary_prefix_byte_cap_accepts_equality_and_rejects_plus_one() {
    let mut created = prompt();
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    created
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            },
        ));
    let exact =
        tau_provider::local_summary_compaction::historical_prefix_json_bytes(&created.context)
            .expect("standalone prompt has a measurable historical prefix");
    let config_with_cap = |cap| {
        let mut config = resolved_provider(&provider());
        config.local_summary_compaction = LocalSummaryCompactionConfig::new(
            NonZeroU64::new(8_192).expect("positive token context"),
            8_192,
            NonZeroU64::new(cap).expect("positive byte cap"),
            NonZeroU32::new(321).expect("positive token output cap"),
            NonZeroU64::new(2_048).expect("positive byte output cap"),
        );
        config
    };

    try_build_request(
        &config_with_cap(exact.get()),
        &provider().models[0],
        &created,
    )
    .expect("exact byte-cap equality is admitted");
    let error = match try_build_request(
        &config_with_cap(exact.get() - 1),
        &provider().models[0],
        &created,
    ) {
        Err(error) => error,
        Ok(_) => panic!("one byte above the independent cap must be rejected"),
    };
    assert!(matches!(error, LlmError::InvalidCompaction(_)));
}

fn image_tool_results_block(call_id: &str, data: impl Into<Arc<[u8]>>) -> tau_proto::ContextBlock {
    tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
        items: vec![tau_proto::ToolResultItem {
            presentation: Default::default(),
            call_id: tau_proto::ToolCallId::new(call_id),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                "image result".to_owned(),
            )),
            provider_content: vec![tau_proto::ToolResultContentPart::Image(
                tau_proto::ImageContent {
                    media_type: tau_proto::ImageMediaType::Png,
                    data: data.into(),
                    width: 17,
                    height: 19,
                    detail: tau_proto::ImageDetail::High,
                },
            )],
        }],
    })
}

/// An explicitly audited llama.cpp route must receive exact multimodal tool
/// content, while the default text-only route must retain a byte-free marker.
#[test]
fn image_tool_result_lowering_is_exact_and_capability_gated() {
    let mut created = prompt();
    created
        .context
        .blocks
        .push(image_tool_results_block("call-image", [11_u8, 22, 33]));

    let config = resolved_provider(&provider());
    let mut model = provider().models[0].clone();
    model.supports_image_tool_results = true;
    let request = try_build_request(&config, &model, &created).expect("vision request");
    assert_eq!(
        request.messages.last(),
        Some(&serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-image",
            "content": [
                {"type": "text", "text": "image result"},
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,CxYh",
                        "detail": "high"
                    }
                }
            ]
        }))
    );

    model.supports_image_tool_results = false;
    let request = try_build_request(&config, &model, &created).expect("text-only request");
    let content = request.messages.last().expect("tool result")["content"]
        .as_str()
        .expect("text-only projection");
    assert!(content.contains("image result"));
    assert!(content.contains("route does not support native image tool output"));
    assert!(!content.contains("CxYh"));
    assert!(
        !serde_json::to_string(&request)
            .expect("request")
            .contains("data:image")
    );
}

/// Provider request diagnostics must preserve useful structure without
/// persisting the canonical image data URL that only the outbound request
/// needs.
#[test]
fn image_tool_result_debug_metadata_redacts_data_urls() {
    let mut created = prompt();
    created
        .context
        .blocks
        .push(image_tool_results_block("call-image", [11_u8, 22, 33]));
    let config = resolved_provider(&provider());
    let mut model = provider().models[0].clone();
    model.supports_image_tool_results = true;
    let request = try_build_request(&config, &model, &created).expect("vision request");

    let debug = provider_request_debug_metadata(&created, &model, &request);
    let debug = serde_json::to_string(&debug).expect("debug metadata");
    assert!(debug.contains("[image data omitted]"));
    assert!(!debug.contains("data:image"));
    assert!(!debug.contains("CxYh"));
}

/// Raw canonical bytes and expanded data URLs are independent provider request
/// limits; exhausting either one must reject the next reservation atomically.
#[test]
fn image_request_budget_enforces_independent_limits() {
    let image = tau_proto::ImageContent {
        media_type: tau_proto::ImageMediaType::Png,
        data: Arc::from([11_u8, 22, 33]),
        width: 17,
        height: 19,
        detail: tau_proto::ImageDetail::High,
    };
    let mut raw_exhausted = ImageRequestBudget {
        supports_image_tool_results: true,
        image_bytes: MAX_REQUEST_IMAGE_BYTES,
        data_url_bytes: 0,
    };
    assert!(!raw_exhausted.reserve(&image));
    assert_eq!(raw_exhausted.image_bytes, MAX_REQUEST_IMAGE_BYTES);
    assert_eq!(raw_exhausted.data_url_bytes, 0);

    let mut expanded_exhausted = ImageRequestBudget {
        supports_image_tool_results: true,
        image_bytes: 0,
        data_url_bytes: MAX_REQUEST_IMAGE_DATA_URL_BYTES,
    };
    assert!(!expanded_exhausted.reserve(&image));
    assert_eq!(expanded_exhausted.image_bytes, 0);
    assert_eq!(
        expanded_exhausted.data_url_bytes,
        MAX_REQUEST_IMAGE_DATA_URL_BYTES
    );
}

/// The aggregate budget must span separate ToolResults blocks so replay cannot
/// reset limits at a transcript boundary and admit a later oversized payload.
#[test]
fn image_request_budget_spans_tool_result_blocks() {
    let mut created = prompt();
    created
        .context
        .blocks
        .push(image_tool_results_block("call-first", vec![1_u8; 2 * 1024]));
    created.context.blocks.push(image_tool_results_block(
        "call-over-limit",
        vec![2_u8; MAX_REQUEST_IMAGE_BYTES - 1024],
    ));
    let config = resolved_provider(&provider());
    let mut model = provider().models[0].clone();
    model.supports_image_tool_results = true;

    let request = try_build_request(&config, &model, &created).expect("bounded vision request");
    let first = &request.messages[1];
    assert_eq!(first["role"], "tool");
    assert_eq!(first["tool_call_id"], "call-first");
    assert_eq!(
        first["content"],
        serde_json::json!([
            {"type": "text", "text": "image result"},
            {
                "type": "image_url",
                "image_url": {
                    "url": format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(vec![1_u8; 2 * 1024])
                    ),
                    "detail": "high"
                }
            }
        ])
    );
    let second = &request.messages[2];
    assert_eq!(second["role"], "tool");
    assert_eq!(second["tool_call_id"], "call-over-limit");
    assert_eq!(
        second["content"],
        serde_json::json!([
            {"type": "text", "text": "image result"},
            {
                "type": "text",
                "text": "[image omitted: aggregate provider image request limit exceeded]"
            }
        ])
    );
}

/// Standalone compaction must reject before request lowering when no selected
/// local-summary configuration enables it.
#[test]
fn standalone_compaction_requires_enabled_local_summary_config() {
    let mut created = prompt();
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut config = resolved_provider(&provider());
    config.local_summary_compaction = None;

    let error = match try_build_request(&config, &provider().models[0], &created) {
        Err(LlmError::InvalidCompaction(error)) => error,
        Ok(_) => panic!("expected standalone-compaction capability gate"),
        Err(error) => panic!("expected standalone-compaction capability gate, got {error:?}"),
    };
    assert_eq!(
        error,
        "standalone compaction is not enabled for this Chat Completions model"
    );
}

/// Standalone lowering must require one exact final harness trigger rather than
/// silently ignoring missing, non-final, duplicated, or mixed markers.
#[test]
fn standalone_compaction_requires_exact_trailing_trigger() {
    let invalid_contexts = [
        Vec::new(),
        vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![assistant_text_item("later")],
            }),
        ],
        vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            }),
        ],
        vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![assistant_text_item("mixed"), ContextItem::CompactionTrigger],
            },
        )],
        vec![
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::CompactionTrigger],
                usage: None,
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            }),
        ],
        vec![tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::CompactionTrigger],
                usage: None,
            },
        )],
    ];
    for blocks in invalid_contexts {
        let mut created = prompt();
        created.operation = tau_proto::PromptOperation::StandaloneCompaction;
        created.context.blocks = blocks;
        assert!(
            matches!(
                try_build_request(
                    &resolved_provider(&provider()),
                    &provider().models[0],
                    &created
                ),
                Err(LlmError::InvalidCompaction(_))
            ),
            "invalid trigger shape must reject before lowering"
        );
    }
}

/// The compact-only event validator accepts one bounded narrative with optional
/// bounded reasoning and an exact `stop` terminal.
#[test]
fn compact_stream_accepts_only_final_narrative_and_reasoning() {
    let mut state =
        StreamState::new_for_attempt(CacheUsageCompat::None, Some(tau_proto::ByteCount::new(15)));
    for event in [
        serde_json::json!({
            "provider": "openai",
            "obfuscation": "padding",
            "choices": [{"index": 0, "delta": {
                "role": "assistant",
                "reasoning_content": "private-private"
            }}]
        }),
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"content": "durable summary"},
                "finish_reason": "stop",
                "native_finish_reason": "end_turn",
                "logprobs": null
            }]
        }),
        serde_json::json!({
            "choices": [],
            "usage": {"completion_tokens": 7}
        }),
    ] {
        apply_event(&mut state, &event, &mut |_| {}).expect("permitted compact event");
    }
    let state = state
        .validate_compaction()
        .expect("complete compact response");
    assert_eq!(
        state.output_items(),
        vec![
            reasoning_text_context_item("private-private").expect("reasoning"),
            assistant_text_item("durable summary"),
        ]
    );
}

/// Tool calls and provider-specific opaque or mixed semantic events must fail
/// before ordinary compatibility parsing can discard or normalize them.
#[test]
fn compact_stream_rejects_tool_opaque_and_mixed_output() {
    for event in [
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{"index": 0, "function": {"name": "shell"}}]}
            }]
        }),
        serde_json::json!({"provider_item": {"opaque": true}}),
        serde_json::json!({
            "choices": [
                {"index": 0, "delta": {"content": "one"}},
                {"index": 1, "delta": {"content": "two"}}
            ]
        }),
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"content": "summary"},
                "logprobs": {"content": []}
            }]
        }),
        serde_json::json!({
            "choices": [{"index": 1, "delta": {"content": "wrong choice"}}]
        }),
        serde_json::json!({
            "error": {"code": "server_error"},
            "choices": [{"index": 0, "delta": {"content": "partial"}}]
        }),
        serde_json::json!({
            "error": {"code": "server_error"},
            "provider_item": {"opaque": true}
        }),
        serde_json::json!({
            "error": {"code": "server_error"},
            "choices": [{"index": 0, "message": {"content": "partial"}}]
        }),
        serde_json::json!({
            "error": {"code": "server_error"},
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": {"bad": true},
                "native_finish_reason": 7,
                "error": ["bad"]
            }]
        }),
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"content": "summary"},
                "finish_reason": "stop",
                "native_finish_reason": 7
            }]
        }),
    ] {
        let mut state = StreamState::new_for_attempt(
            CacheUsageCompat::None,
            Some(tau_proto::ByteCount::new(64)),
        );
        assert!(matches!(
            apply_event(&mut state, &event, &mut |_| {}),
            Err(LlmError::InvalidCompaction(_))
        ));
    }
}

/// Unknown provider fields remain ordinary-inference compatibility metadata,
/// while the compact-only language rejects the same opaque event.
#[test]
fn compact_only_opaque_rejection_preserves_ordinary_compatibility() {
    let event = serde_json::json!({"provider_item": {"opaque": true}});
    let mut ordinary = StreamState::new();
    apply_event(&mut ordinary, &event, &mut |_| {}).expect("ordinary parser ignores unknown field");
    assert!(ordinary.output_items().is_empty());
    assert_eq!(ordinary.semantic_progress, SemanticProgress::None);

    let mut compact =
        StreamState::new_for_attempt(CacheUsageCompat::None, Some(tau_proto::ByteCount::new(64)));
    assert!(matches!(
        apply_event(&mut compact, &event, &mut |_| {}),
        Err(LlmError::InvalidCompaction(_))
    ));
}

/// Compact rejection must not suppress the existing transient progress
/// callback; the separately approved extension-publication change owns that
/// boundary.
#[test]
fn compact_rejection_preserves_ordinary_transient_progress_callback() {
    let mut state =
        StreamState::new_for_attempt(CacheUsageCompat::None, Some(tau_proto::ByteCount::new(64)));
    let mut observed = SemanticProgress::None;
    let result = apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": "tentative",
                    "tool_calls": [{
                        "index": 0,
                        "function": {"name": "shell", "arguments": "{}"}
                    }]
                }
            }]
        }),
        &mut |state| observed = state.semantic_progress,
    );
    assert!(matches!(result, Err(LlmError::InvalidCompaction(_))));
    assert_eq!(observed, SemanticProgress::Parsed);
}

/// Compact-shape rejection remains the existing deterministic request-rejected
/// terminal rather than becoming a retry or an unclassified failure.
#[test]
fn compact_rejection_is_a_classified_terminal_attempt() {
    let outcome = finish_attempt(
        Err(LlmError::InvalidCompaction(
            "invalid compact shape".to_owned(),
        )),
        SemanticProgress::Parsed,
    );
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("invalid compact shape must terminalize");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert_eq!(failure.stop_reason, ProviderStopReason::Error);
}

/// Missing, duplicated, wrong, and non-final terminals must not turn
/// parser-compatible partial output into a successful compact response.
#[test]
fn compact_stream_requires_one_final_stop_terminal() {
    let cases = [
        vec![serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "unterminated"}}]
        })],
        vec![serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {"content": "truncated"},
                "finish_reason": "length"
            }]
        })],
        vec![
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"content": "first"},
                    "finish_reason": "stop"
                }]
            }),
            serde_json::json!({
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            }),
        ],
        vec![
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"content": "early"},
                    "finish_reason": "stop"
                }]
            }),
            serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": "late"}}]
            }),
        ],
    ];
    for events in cases {
        let mut state = StreamState::new_for_attempt(
            CacheUsageCompat::None,
            Some(tau_proto::ByteCount::new(64)),
        );
        let mut rejected = false;
        for event in events {
            if matches!(
                apply_event(&mut state, &event, &mut |_| {}),
                Err(LlmError::InvalidCompaction(_))
            ) {
                rejected = true;
                break;
            }
        }
        if !rejected {
            rejected = matches!(
                state.validate_compaction(),
                Err(LlmError::InvalidCompaction(_))
            );
        }
        assert!(rejected, "invalid compact terminal shape must reject");
    }
}

/// Narrative and reasoning consume separate compact byte budgets, and neither
/// channel may exceed the selected bound.
#[test]
fn compact_stream_bounds_narrative_and_reasoning_separately() {
    for delta in [
        serde_json::json!({"content": "12345"}),
        serde_json::json!({"reasoning_content": "12345", "content": "ok"}),
    ] {
        let mut state = StreamState::new_for_attempt(
            CacheUsageCompat::None,
            Some(tau_proto::ByteCount::new(4)),
        );
        apply_event(
            &mut state,
            &serde_json::json!({
                "choices": [{"index": 0, "delta": delta, "finish_reason": "stop"}]
            }),
            &mut |_| {},
        )
        .expect("shape is valid before complete-output bounds");
        assert!(matches!(
            state.validate_compaction(),
            Err(LlmError::InvalidCompaction(_))
        ));
    }
}

/// Standalone local compaction must use the ordinary durable provider
/// diagnostics policy.
#[test]
fn local_summary_compaction_uses_ordinary_request_debug_capture() {
    let mut created = prompt();
    assert!(debug_capture_enabled_for_prompt(&created, true));
    created.operation = tau_proto::PromptOperation::StandaloneCompaction;
    assert!(debug_capture_enabled_for_prompt(&created, true));
    assert!(!debug_capture_enabled_for_prompt(&created, false));
}

/// The configured raw narrative limit cannot exceed the harness's fixed
/// composite-checkpoint memory and persistence budget.
#[test]
fn local_summary_compaction_rejects_output_bytes_above_harness_narrative_cap() {
    let context = NonZeroU64::new(1_000_000).expect("positive");
    let input = NonZeroU64::new(256).expect("positive");
    let output_tokens = NonZeroU32::new(1).expect("positive");
    let exact =
        NonZeroU64::new(tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64).expect("positive");
    assert!(
        LocalSummaryCompactionConfig::new(context, context.get(), input, output_tokens, exact)
            .is_some()
    );
    let over = NonZeroU64::new(exact.get() + 1).expect("positive");
    assert!(
        LocalSummaryCompactionConfig::new(context, context.get(), input, output_tokens, over)
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

    let mut qwen_provider = provider;
    qwen_provider.compat.reasoning_replay = ReasoningReplay::Both;
    let request = build_request(
        &resolved_provider(&qwen_provider),
        &qwen_provider.models[0],
        &replay,
    );
    let json = serde_json::to_value(request).expect("Qwen request json");
    assert_eq!(
        json["messages"][0]["reasoning_content"],
        "need current date"
    );
    assert_eq!(json["messages"][0]["reasoning"], "need current date");
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

/// Sender route context keeps the original function arguments and compact tool
/// result, but must not lower a routed body as provider assistant output.
#[test]
fn message_route_context_omits_outbound_body_from_assistant_wire_content() {
    const BODY: &str = "CLANK2AE7_CHAT_COMPLETIONS_CANARY";
    let mut replay = prompt();
    replay.context = tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: tau_proto::ToolCallId::new("message-call"),
                    name: tau_proto::ToolName::new("message"),
                    tool_type: ToolType::Function,
                    arguments: tau_proto::json_to_cbor(&serde_json::json!({
                        "recipient_id": "recipient",
                        "message": BODY,
                    })),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                usage: None,
            }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: tau_proto::ToolCallId::new("message-call"),
                    tool_type: ToolType::Function,
                    status: ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                        "Message sent".to_owned(),
                    )),
                    provider_content: Vec::new(),
                }],
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(tau_proto::MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text {
                        text: format!("<tau_internal>received {BODY}&lt;/tau_internal&gt;"),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
        ],
    };
    let provider = provider();
    let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
    let request = serde_json::to_value(request).expect("request json");

    let messages = request["messages"].as_array().expect("messages array");
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant"
            && message["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .is_some_and(|arguments| arguments.contains(BODY))
    }));
    assert!(
        messages
            .iter()
            .any(|message| message["role"] == "tool" && message["content"] == "Message sent")
    );
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains(BODY))
    }));
    assert!(
        !messages.iter().any(|message| {
            message["role"] == "assistant" && message["content"].to_string().contains(BODY)
        }),
        "the Chat Completions request must not contain a synthetic assistant replay"
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

/// Qwen3.8 accepts literal `xhigh`, unlike OpenAI-compatible routes that fold
/// extended Tau effort levels to `high`; each configured Qwen level must retain
/// its exact wire spelling.
#[test]
fn qwen_reasoning_efforts_use_literal_wire_spellings() {
    let mut provider = provider();
    provider.compat.reasoning_effort = Some(ReasoningEffortWire::Literal);
    for (effort, expected) in [
        (tau_proto::Effort::Low, "low"),
        (tau_proto::Effort::Medium, "medium"),
        (tau_proto::Effort::XHigh, "xhigh"),
    ] {
        let mut prompt = prompt();
        prompt.model_params.effort = effort;
        let request = build_request(&resolved_provider(&provider), &provider.models[0], &prompt);
        let json = serde_json::to_value(request).expect("request json");
        assert_eq!(json["reasoning_effort"], expected);
    }
}

/// Qwen non-thinking is a template choice rather than an `off`
/// `reasoning_effort`; the profile must pass `enable_thinking: false` without
/// publishing or sending an unsupported effort value.
#[test]
fn qwen_non_thinking_uses_template_switch_without_reasoning_effort() {
    let mut provider = provider();
    provider.compat.reasoning_effort = None;
    provider.extra_body.insert(
        "chat_template_kwargs".to_owned(),
        serde_json::json!({"enable_thinking": false}),
    );
    provider
        .extra_body
        .insert("temperature".to_owned(), serde_json::json!(0.7));
    provider
        .extra_body
        .insert("top_p".to_owned(), serde_json::json!(0.8));
    provider
        .extra_body
        .insert("top_k".to_owned(), serde_json::json!(20));
    provider
        .extra_body
        .insert("min_p".to_owned(), serde_json::json!(0.0));

    let request = build_request(
        &resolved_provider(&provider),
        &provider.models[0],
        &prompt(),
    );
    let json = serde_json::to_value(request).expect("request json");
    assert!(json.get("reasoning_effort").is_none());
    assert_eq!(
        json["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": false})
    );
    assert_eq!(json["temperature"], 0.7);
    assert_eq!(json["top_p"], 0.8);
    assert_eq!(json["top_k"], 20);
    assert_eq!(json["min_p"], 0.0);
}

/// Qwen's checked-in template permits system authority only before all ordinary
/// transcript messages, so an opted-in route must reject later system or
/// developer messages locally instead of sending a deterministic server error.
#[test]
fn qwen_single_system_compat_rejects_later_system_authority() {
    for role in [ContextRole::System, ContextRole::Developer] {
        let mut provider = provider();
        provider.compat.single_initial_system_message = true;
        let mut prompt = prompt();
        prompt.system_prompt = "leading authority".to_owned();
        let tau_proto::ContextBlock::UserInput(block) = &mut prompt.context.blocks[0] else {
            panic!("fixture begins with user input");
        };
        block
            .items
            .push(ContextItem::Message(tau_proto::MessageItem {
                role,
                content: vec![ContentPart::Text {
                    text: "later authority".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }));
        let result = try_build_request(&resolved_provider(&provider), &provider.models[0], &prompt);
        assert!(matches!(result, Err(LlmError::UnsupportedMessageRole)));
    }
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
        "prompt_cache_retention",
        "prompt_cache_options",
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

/// Full reasoning from a length-stopped response must replay as the immediately
/// preceding assistant item before Tau's exact reserved user instruction.
#[test]
fn full_reasoning_length_replays_exactly_before_continuation_instruction() {
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "reasoning_content": "retained exact reasoning" },
                "finish_reason": "length"
            }]
        }),
        &mut |_| {},
    )
    .expect("length stream event");
    assert_eq!(state.stop_reason, ProviderStopReason::Length);
    let items = state.output_items();
    assert!(matches!(
        items.as_slice(),
        [ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text,
        })] if text == "retained exact reasoning"
    ));

    for replay_mode in [
        ReasoningReplay::ReasoningContent,
        ReasoningReplay::Reasoning,
        ReasoningReplay::Both,
    ] {
        let mut replay = prompt();
        replay.context = tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: None,
                    backend: None,
                    output_items: items.clone(),
                    usage: None,
                }),
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(tau_proto::MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                }),
            ],
        };
        let mut provider = provider();
        provider.compat.reasoning_replay = replay_mode;
        let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
        let json = serde_json::to_value(request).expect("request json");
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert!(messages[0]["content"].is_null());
        assert!(messages[0]["tool_calls"].is_null());
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
        );
        assert_eq!(
            messages[0]["reasoning_content"].as_str(),
            matches!(
                replay_mode,
                ReasoningReplay::ReasoningContent | ReasoningReplay::Both
            )
            .then_some("retained exact reasoning")
        );
        assert_eq!(
            messages[0]["reasoning"].as_str(),
            matches!(
                replay_mode,
                ReasoningReplay::Reasoning | ReasoningReplay::Both
            )
            .then_some("retained exact reasoning")
        );
    }
}

/// Summary-only reasoning from a length-stopped response must never become
/// replay authority: the rebuilt request carries no assistant reasoning or
/// content before Tau's exact reserved user instruction.
#[test]
fn summary_only_length_reasoning_has_no_replay_authority() {
    let mut state = StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "choices": [{
                "delta": { "reasoning_content": "summary reasoning" },
                "finish_reason": "length"
            }]
        }),
        &mut |_| {},
    )
    .expect("length stream event");
    assert_eq!(state.stop_reason, ProviderStopReason::Length);
    let items = state.output_items();
    // The adapter exposes streamed reasoning as Full by default in this
    // fixture; replace it with the summary form the harness marks ineligible.
    let items = items
        .iter()
        .map(|item| match item {
            ContextItem::ReasoningText(reasoning) => {
                ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                    kind: tau_proto::ReasoningTextKind::Summary,
                    ..reasoning.clone()
                })
            }
            other => other.clone(),
        })
        .collect::<Vec<_>>();
    for replay_mode in [
        ReasoningReplay::ReasoningContent,
        ReasoningReplay::Reasoning,
        ReasoningReplay::Both,
    ] {
        let mut replay = prompt();
        replay.context = tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: None,
                    backend: None,
                    output_items: items.clone(),
                    usage: None,
                }),
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(tau_proto::MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                }),
            ],
        };
        let mut provider = provider();
        provider.compat.reasoning_replay = replay_mode;
        let request = build_request(&resolved_provider(&provider), &provider.models[0], &replay);
        let json = serde_json::to_value(request).expect("request json");
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            1,
            "summary-only reasoning emits no assistant replay message in mode {replay_mode:?}"
        );
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"],
            tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
        );
    }
}

/// A non-null provider terminal outside Tau's supported stop contract must fail
/// explicitly rather than inheriting the stream state's default successful
/// end-turn classification.
#[test]
fn unknown_non_null_finish_reason_is_not_a_success() {
    for finish_reason in [serde_json::json!("content_filter"), serde_json::json!(17)] {
        let mut state = StreamState::new();
        let result = apply_event(
            &mut state,
            &serde_json::json!({
                "choices": [{
                    "delta": {"content": "tentative"},
                    "finish_reason": finish_reason
                }]
            }),
            &mut |_| {},
        );
        assert!(matches!(result, Err(LlmError::UnknownFinishReason)));
    }
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

/// Ensures the HTTP status table terminalizes every unauthorized 4xx while
/// retaining each explicitly authorized retry status.
#[test]
fn http_status_classification_is_closed() {
    for (status, expected) in numeric_status_cases() {
        let error = LlmError::HttpStatus(status, r#"{"error":{"code":"unknown"}}"#.to_owned());
        match expected {
            NumericStatusExpectation::Terminal => {
                assert_eq!(
                    error.retry_decision(),
                    None,
                    "HTTP {status} must terminalize"
                );
                assert_eq!(
                    error.failure_kind(),
                    Some(tau_proto::ProviderFailureKind::RequestRejected),
                    "HTTP {status} must reject the unchanged request"
                );
            }
            NumericStatusExpectation::Retry(class) => {
                assert_eq!(
                    error
                        .retry_decision()
                        .expect("authorized status must retry")
                        .class,
                    class,
                    "HTTP {status} retry class"
                );
                assert_eq!(
                    error.failure_kind(),
                    None,
                    "HTTP {status} must not terminalize"
                );
            }
        }
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
/// Canonical context identifiers terminalize even when the HTTP status would
/// otherwise request a throttled retry.
#[test]
fn canonical_context_error_bypasses_retry_scheduler() {
    for body in [
        r#"{"error":{"code":"context_length_exceeded","type":"rate_limit_exceeded"}}"#,
        r#"{"error":{"code":"rate_limit_exceeded","type":"context_length_exceeded"}}"#,
    ] {
        let error = LlmError::HttpStatus(429, body.to_owned());
        assert_eq!(error.retry_decision(), None);
        assert_eq!(
            error.failure_kind(),
            Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
        );
    }
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

/// Ensures each exact approved identifier overrides deterministic status
/// through every reviewed HTTP and streamed authority path.
#[test]
fn approved_structured_identifiers_override_deterministic_status() {
    for (identifier, expected) in [
        ("usage_limit_reached", RetryClass::UsageWindow),
        ("rate_limit_exceeded", RetryClass::Throttle),
        ("quota_exceeded", RetryClass::Account),
        ("billing_hard_limit_reached", RetryClass::Account),
        ("insufficient_quota", RetryClass::Account),
        ("usage_not_included", RetryClass::Account),
        ("credits_exhausted", RetryClass::Account),
        ("invalid_api_key", RetryClass::Auth),
        ("authentication_error", RetryClass::Auth),
        ("invalid_authentication", RetryClass::Auth),
        ("token_expired", RetryClass::Auth),
        ("unauthorized", RetryClass::Auth),
        ("overloaded_error", RetryClass::Overload),
        ("server_error", RetryClass::Overload),
        ("upstream_timeout", RetryClass::Overload),
    ] {
        for field in ["code", "type"] {
            let body = format!(r#"{{"error":{{"{field}":"{identifier}"}}}}"#);
            let http = LlmError::HttpStatus(405, body);
            assert_eq!(
                http.retry_decision()
                    .expect("approved canonical identifier must retry")
                    .class,
                expected,
                "HTTP {field} identifier {identifier}"
            );
            assert_eq!(
                http.failure_kind(),
                None,
                "HTTP {field} identifier {identifier}"
            );
        }

        for (path, error) in [
            ("code", serde_json::json!({"code": identifier})),
            ("type", serde_json::json!({"code": 415, "type": identifier})),
            (
                "metadata.error_type",
                serde_json::json!({
                    "code": 415,
                    "metadata": {"error_type": identifier}
                }),
            ),
        ] {
            let streamed = classify_stream_error(error.as_object().expect("streamed error object"));
            assert_eq!(
                streamed
                    .retry
                    .expect("approved streamed identifier must retry")
                    .class,
                expected,
                "streamed {path} identifier {identifier}"
            );
            assert_eq!(
                streamed.failure_kind, None,
                "streamed {path} identifier {identifier}"
            );
        }
    }
}

/// Ensures unreviewed recursive HTTP metadata and provider prose cannot turn a
/// deterministic rejection into a retry.
#[test]
fn deterministic_http_status_ignores_unreviewed_structured_paths_and_prose() {
    for body in [
        r#"{"error":{"metadata":{"error_type":"rate_limit_exceeded"}}}"#,
        r#"{"error":{"message":"temporary upstream failure"},"echo":{"code":"rate_limit_exceeded"}}"#,
        r#"{"error":{"message":"temporary (type=content_policy_violation)"}}"#,
    ] {
        assert_eq!(retry_decision_for_http_error(405, body, None), None);
        assert_eq!(
            http_failure_kind(405, body),
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        );
    }
}

/// Ensures a reviewed streamed context identifier beats numeric throttling and
/// never becomes assistant transcript text.
#[test]
fn streamed_context_error_is_typed_terminal_without_assistant_output() {
    let mut state = StreamState::new();
    let error = apply_event(
        &mut state,
        &serde_json::json!({
            "error": {
                "code": 429,
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

/// Ensures every reviewed streamed context path takes precedence over a known
/// retry identifier and numeric throttling where that status is representable.
#[test]
fn streamed_context_identifier_paths_bypass_retry_scheduler() {
    for (path, error) in [
        (
            "code",
            serde_json::json!({
                "code": "context_length_exceeded",
                "type": "rate_limit_exceeded"
            }),
        ),
        (
            "type",
            serde_json::json!({
                "code": 429,
                "type": "context_length_exceeded",
                "metadata": {"error_type": "rate_limit_exceeded"}
            }),
        ),
        (
            "metadata.error_type",
            serde_json::json!({
                "code": 429,
                "type": "rate_limit_exceeded",
                "metadata": {"error_type": "context_length_exceeded"}
            }),
        ),
    ] {
        let failure = classify_stream_error(error.as_object().expect("streamed error object"));
        assert_eq!(
            failure.retry, None,
            "streamed {path} context must terminalize"
        );
        assert_eq!(
            failure.failure_kind,
            Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
            "streamed {path} context must override retry signals"
        );
    }
}

/// Ensures only the reviewed streamed metadata path and exact identifiers can
/// classify context recovery; arbitrary metadata and provider prose stay
/// unknown.
#[test]
fn streamed_error_ignores_unknown_nested_metadata_and_prose() {
    for error in [
        serde_json::json!({
            "metadata": {
                "error_type": "unknown_provider_error",
                "nested": {"error_type": "context_length_exceeded"}
            },
            "message": "context_length_exceeded"
        }),
        serde_json::json!({
            "metadata": {"context_length_exceeded": true},
            "message": "context_length_exceeded"
        }),
    ] {
        let failure = classify_stream_error(error.as_object().expect("error object"));
        assert_eq!(failure.failure_kind, None);
        assert_eq!(
            failure
                .retry
                .expect("unknown streamed error is retryable")
                .class,
            RetryClass::Unknown
        );
    }
}

/// Ensures the reviewed streamed metadata path maps an allowlisted retry
/// identifier, while provider-authored prose remains outside the outcome.
#[test]
fn streamed_allowlisted_metadata_retry_error_is_typed_retryable() {
    let mut state = StreamState::new();
    let error = apply_event(
        &mut state,
        &serde_json::json!({
            "error": {
                "code": 400,
                "metadata": {"error_type": "rate_limit_exceeded"},
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
        panic!("allowlisted streamed metadata must be retryable");
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

/// Ensures streamed numeric statuses use the same closed table as HTTP
/// responses, including 405 and 415 deterministic request rejections.
#[test]
fn streamed_numeric_status_classification_is_closed() {
    for (status, expected) in numeric_status_cases() {
        let failure = classify_stream_error(
            serde_json::json!({"code": status, "message": "ignored"})
                .as_object()
                .expect("streamed error object"),
        );
        match expected {
            NumericStatusExpectation::Terminal => {
                assert_eq!(failure.retry, None, "streamed {status} must terminalize");
                assert_eq!(
                    failure.failure_kind,
                    Some(tau_proto::ProviderFailureKind::RequestRejected),
                    "streamed {status} must reject the unchanged request"
                );
            }
            NumericStatusExpectation::Retry(class) => {
                assert_eq!(
                    failure
                        .retry
                        .expect("authorized streamed status must retry")
                        .class,
                    class,
                    "streamed {status} retry class"
                );
                assert_eq!(
                    failure.failure_kind, None,
                    "streamed {status} must not terminalize"
                );
            }
        }
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
