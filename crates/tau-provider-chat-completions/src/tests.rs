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

/// Reads through the HTTP request line without assuming one TCP read contains
/// it.
fn read_request_line(socket: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while !bytes.ends_with(b"\r\n") {
        assert!(
            bytes.len() < 8 * 1024,
            "fixture request line must remain bounded"
        );
        socket.read_exact(&mut byte).expect("read request line");
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).expect("ASCII HTTP request line")
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

/// Ensures the production reqwest transport consumes an actual local HTTP/SSE
/// response and returns a successful one-attempt outcome.
#[test]
fn reqwest_transport_streams_local_success_response() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().expect("server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request).expect("read request");
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},",
            "\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    });
    let mut configured = provider();
    configured.base_url = format!("http://{address}");
    let model = configured.models[0].clone();
    let prompt = prompt();
    let mut bytes = Vec::new();
    let mut writer = PeerOutputWriter::new(&mut bytes);
    let outcome = run_prompt_attempt_for_provider(
        &prompt.agent_prompt_id,
        &prompt,
        &configured,
        &model,
        false,
        &mut writer,
        &mut || false,
    );
    let PromptAttemptOutcome::Finished(finished) = outcome else {
        panic!("local success must finish");
    };
    assert_eq!(finished.stop_reason, ProviderStopReason::EndTurn);
    assert!(!finished.output_items.is_empty());
    server.join().expect("server join");
}

/// Ensures both generic Chat Completions and the OpenRouter compatibility route
/// turn a real local 429 response into the same scheduler-owned throttle retry.
#[test]
fn local_http_throttle_contract_covers_generic_and_openrouter_routes() {
    use std::io::Write as _;

    let model = provider().models[0].clone();
    let openrouter = crate::openrouter::OpenRouterProfile {
        api_key: "fixture-openrouter-key".to_owned(),
        models: vec![model.clone()],
    }
    .to_chat_completions();
    for mut configured in [provider(), openrouter] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("server address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            assert!(
                read_request_line(&mut socket).contains("/chat/completions"),
                "attempt must traverse the production Chat Completions route"
            );
            let body = r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#;
            write!(
                socket,
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 37\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });
        configured.base_url = format!("http://{address}");
        let configured_model = configured.models[0].clone();
        let prompt = prompt();
        let mut bytes = Vec::new();
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let outcome = run_prompt_attempt_for_provider(
            &prompt.agent_prompt_id,
            &prompt,
            &configured,
            &configured_model,
            false,
            &mut writer,
            &mut || false,
        );
        let PromptAttemptOutcome::Retry(decision) = outcome else {
            panic!("canonical local 429 must remain scheduler-owned");
        };
        assert_eq!(decision.class, RetryClass::Throttle);
        assert_eq!(decision.retry_after, Some(Duration::from_secs(37)));
        server.join().expect("server join");
    }
}

/// Ensures Chat Completions streaming updates emit append deltas rather than
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
        std::io::Cursor::new(bytes),
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
        std::io::Cursor::new(b"data: never read\n\n"),
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
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind cancellation server");
    let address = listener.local_addr().expect("cancellation server address");
    let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
    let (dropped_tx, dropped_rx) = std::sync::mpsc::sync_channel(1);
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

    let canceled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attempt_canceled = std::sync::Arc::clone(&canceled);
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let attempt = std::thread::spawn(move || {
        let mut configured = provider();
        configured.base_url = format!("http://{address}/v1");
        let model = configured.models[0].clone();
        let prompt = prompt();
        let mut bytes = Vec::new();
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        let outcome = run_prompt_attempt_for_provider(
            &"ap-cancel".into(),
            &prompt,
            &configured,
            &model,
            false,
            &mut writer,
            &mut || attempt_canceled.load(std::sync::atomic::Ordering::SeqCst),
        );
        result_tx.send(outcome).expect("report attempt outcome");
    });
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reqwest request did not reach local peer");
    canceled.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("canceled reqwest attempt stayed blocked"),
        PromptAttemptOutcome::Canceled
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

/// Ensures a malicious provider cannot grow the pending SSE-line buffer
/// without bound by streaming bytes forever without a newline.
#[test]
fn chat_stream_body_rejects_oversized_partial_line() {
    let bytes = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    let mut state = StreamState::new();
    let mut raw_events = Vec::new();
    let error = read_chat_stream_body(
        std::io::Cursor::new(bytes),
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
        std::io::Cursor::new(bytes),
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

/// Ensures the provider emission boundary does not suppress public stats-only
/// updates that have no displayable text deltas.
#[test]
fn stream_update_emits_response_stats_without_text_deltas() {
    let mut state = StreamState::new();
    state
        .append_tool_arguments_delta(0, "{\"cmd\":\"ls\"}")
        .expect("argument delta");
    let mut bytes = Vec::new();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut delta_emitter = StreamDeltaEmitter::default();
        emit_stream_update(
            &AgentPromptId::from("sp-tool-args"),
            &prompt(),
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
    let Some(HarnessInputMessage::Emit(emit)) = frames.first() else {
        panic!("expected provider response update frame: {frames:?}");
    };
    let Event::ProviderResponseUpdated(update) = emit.event.as_ref() else {
        panic!("expected provider response update: {:?}", emit.event);
    };
    assert!(update.deltas.is_empty());
    assert_eq!(
        update
            .response_stats
            .as_ref()
            .map(|stats| stats.current.response_bytes_received),
        Some("{\"cmd\":\"ls\"}".len() as u64),
    );
}

/// Ensures Chat Completions progress frames publish the first streamed chunk
/// promptly, then follow provider-prompt cadence instead of emitting once per
/// upstream chunk or byte change.
#[test]
fn response_update_emitter_rate_limits_non_terminal_updates() {
    let prompt = prompt();
    let agent_prompt_id = AgentPromptId::from("sp-rate-limit");
    let mut state = StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);

        state
            .append_assistant_text_delta("hel")
            .expect("first text delta");
        emitter.emit_at(&agent_prompt_id, &prompt, &state, &mut writer, start, false);
        state
            .append_assistant_text_delta("lo")
            .expect("second text delta");
        state
            .append_tool_arguments_delta(0, "{\"cmd\":\"ls\"}")
            .expect("tool argument delta");
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderResponseUpdated(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "updates: {updates:#?}");
    assert_eq!(
        updates[0].deltas,
        vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "hel".to_owned(),
            phase: None,
        },]
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
        vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "lo".to_owned(),
            phase: None,
        },]
    );
    assert_eq!(
        updates[1].response_stats,
        Some(ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: ("hello".len() + "{\"cmd\":\"ls\"}".len()) as u64,
                elapsed_micros: 1_000_000,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: "hel".len() as u64,
                elapsed_micros: 0,
            },
        })
    );
}

/// Ensures due provider response samples are emitted even when no bytes
/// changed, so the last emitted sample remains the `previous` point for
/// stateless UI interval-rate calculations.
#[test]
fn response_update_emitter_emits_due_stats_only_sample() {
    let prompt = prompt();
    let agent_prompt_id = AgentPromptId::from("sp-stats-only");
    let state = StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL * 2,
            false,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderResponseUpdated(update) => Some(update),
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
fn response_update_emitter_emits_first_bytes_after_idle_sample_promptly() {
    let prompt = prompt();
    let agent_prompt_id = AgentPromptId::from("sp-first-bytes-after-idle");
    let mut state = StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            false,
        );
        state
            .append_assistant_text_delta("hi")
            .expect("first text delta");
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            false,
        );
        state
            .append_assistant_text_delta("!")
            .expect("second text delta");
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2
                + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 4,
            false,
        );
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
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
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderResponseUpdated(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 3, "updates: {updates:#?}");
    assert!(updates[0].deltas.is_empty());
    assert_eq!(
        updates[1].deltas,
        vec![ProviderResponseTextDelta::Message {
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
        vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "!".to_owned(),
            phase: None,
        }]
    );
}

/// Ensures the first non-empty progress bypass applies to stats-only tool
/// argument bytes, not just visible assistant text.
#[test]
fn response_update_emitter_emits_first_stats_only_sample_promptly() {
    let prompt = prompt();
    let agent_prompt_id = AgentPromptId::from("sp-first-semantic-output");
    let mut state = StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        state
            .append_tool_arguments_delta(0, "{\"cmd\":\"ls\"}")
            .expect("tool argument delta");
        emitter.emit_at(&agent_prompt_id, &prompt, &state, &mut writer, start, false);
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderResponseUpdated(update) => Some(update),
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
        "{\"cmd\":\"ls\"}".len() as u64
    );
}

/// Ensures a terminal flush can publish the final suffix immediately before
/// `provider.response_finished`, without losing text suppressed by the
/// non-terminal one-second cadence after the first streamed chunk.
#[test]
fn response_update_emitter_terminal_flush_emits_batched_suffix() {
    let prompt = prompt();
    let agent_prompt_id = AgentPromptId::from("sp-terminal-flush");
    let mut state = StreamState::new();
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        let mut emitter = RateLimitedResponseUpdateEmitter::new_at(start);
        state
            .append_assistant_text_delta("hel")
            .expect("first text delta");
        emitter.emit_at(&agent_prompt_id, &prompt, &state, &mut writer, start, false);
        state
            .append_assistant_text_delta("lo")
            .expect("second text delta");
        emitter.emit_at(
            &agent_prompt_id,
            &prompt,
            &state,
            &mut writer,
            start + PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL / 2,
            true,
        );
    }

    let updates: Vec<_> = decode_frames(&bytes)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderResponseUpdated(update) => Some(update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "updates: {updates:#?}");
    assert_eq!(
        updates[0].deltas,
        vec![ProviderResponseTextDelta::Message {
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
        vec![ProviderResponseTextDelta::Message {
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
            retry: None,
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
        operation: tau_proto::PromptOperation::Inference,
    }
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
            ..
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
