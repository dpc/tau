//! Oracles for the provider-neutral pre-lowered SSE boundary.

use serde_json::value::RawValue;
use tokio::net::TcpListener as AsyncTcpListener;

use super::*;
use crate::cache_diagnostic::tests::collect;

/// Account-bound adapters can disable hidden transport replay while ordinary
/// generic and other prepared routes retain their previous client policy.
#[test]
fn prepared_transport_retry_policy_is_explicit_and_not_sent() {
    let raw = RawValue::from_string(r#"{"model":"fixture","input":[],"stream":true}"#.into())
        .expect("raw");
    let ordinary = PreparedSseRequest::from_json(raw.clone()).expect("ordinary");
    assert!(!ordinary.without_transport_retries);
    let restricted = PreparedSseRequest::from_json(raw)
        .expect("restricted")
        .without_transport_retries()
        .with_required_completed_event();
    assert!(restricted.without_transport_retries);
    assert!(restricted.require_response_completed);
    assert_eq!(ordinary.json().get(), restricted.json().get());
}

/// Restricted plan routes must not confuse done aliases or partial output with
/// canonical success, while generic Responses retains its existing behavior.
#[test]
fn required_completed_event_rejects_done_and_error_terminals() {
    for terminal in [
        "response.completed",
        "response.done",
        "response.failed",
        "response.incomplete",
    ] {
        let mut state = State {
            require_response_completed: true,
            non_retryable_incomplete_reasons: &["max_output_tokens"],
            ..Default::default()
        };
        let event = serde_json::json!({
            "type":terminal,
            "response":{"id":"response-fixture","output":[],
                "incomplete_details":{"reason":"max_output_tokens"},
                "error":{"code":"subscription_sharing_usage_limit_exceeded"}}
        });
        let result = state.apply_event(&event.to_string());
        match terminal {
            "response.completed" => assert_eq!(state.terminal, Some(TerminalKind::Completed)),
            "response.incomplete" => {
                assert!(result.is_ok());
                assert_eq!(
                    state.terminal,
                    Some(TerminalKind::NonRetryableIncomplete("max_output_tokens"))
                );
            }
            _ => {
                assert!(result.is_err());
                assert_eq!(state.terminal, None);
            }
        }
    }
    let mut generic = State::default();
    generic
        .apply_event(r#"{"type":"response.done","response":{"output":[]}}"#)
        .expect("legacy generic alias");
    assert_eq!(generic.terminal, Some(TerminalKind::Completed));
}

/// Grok's prepared-only limit policy never changes ordinary generic attempts.
#[test]
fn generic_incomplete_limits_keep_existing_retry_classification() {
    for reason in ["max_prompt_tokens", "max_time_limit"] {
        let mut state = State::default();
        let event = serde_json::json!({
            "type":"response.incomplete",
            "response":{"incomplete_details":{"reason":reason},"output":[]}
        });
        assert!(matches!(
            state.apply_event(&event.to_string()),
            Err(Error::SseProvider {
                retry_class: RetryClass::Unknown,
                ..
            })
        ));
        assert_eq!(state.terminal, None);
    }
}

/// Minimal transport config without provider-specific cache controls.
fn config() -> AttemptConfig {
    AttemptConfig {
        base_url: "http://example.invalid".into(),
        api_key: "credential-canary".into(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    }
}

/// Model identity shared by the request envelope and finite attempt.
fn model() -> AttemptModel {
    AttemptModel {
        id: ModelName::new("test-model"),
    }
}

/// Parse test input while retaining its exact wire bytes.
fn prepared(json: &str) -> Result<PreparedSseRequest, PrepareSseRequestError> {
    PreparedSseRequest::from_json(RawValue::from_string(json.into()).expect("test JSON"))
}

/// Generic lowering still serializes identically through either request source.
#[test]
fn prepared_lowering_preserves_generic_request_bytes() {
    let prompt = cache_prefix_prompt();
    let config = config();
    let model = model();
    let ordinary = build_request(&prompt, &config, &model).expect("ordinary request");
    let prepared = PreparedSseRequest::lower(&prompt, &config, &model).expect("prepared request");
    assert_eq!(
        serde_json::to_string(&ordinary).expect("ordinary JSON"),
        prepared.json().get()
    );
    assert_eq!(
        serde_json::to_string(&AttemptRequest::Standard(ordinary)).expect("standard source"),
        serde_json::to_string(&AttemptRequest::Prepared(&prepared)).expect("prepared source")
    );
}

/// The neutral seam rejects chaining and malformed SSE envelopes locally.
#[test]
fn prepared_request_rejects_non_full_replay_envelopes() {
    for json in [
        r#"[]"#,
        r#"{"model":"test-model","input":[],"stream":false}"#,
        r#"{"model":"test-model","input":[]}"#,
        r#"{"model":"","input":[],"stream":true}"#,
        r#"{"model":"test-model","input":{},"stream":true}"#,
        r#"{"model":"test-model","input":[],"stream":true,"previous_response_id":null}"#,
        r#"{"model":"test-model","input":[],"stream":true,"previous_response_id":"secret"}"#,
    ] {
        let error = prepared(json).err().expect("invalid envelope");
        assert_eq!(error.to_string(), "invalid prepared Responses SSE request");
        assert!(!format!("{error:?}").contains("secret"));
    }
}

/// Preflight rejection and cancellation never emit dispatch observations.
#[test]
fn prepared_request_rejects_model_and_transport_mismatch_before_dispatch() {
    let request = prepared(r#"{"model":"test-model","input":[],"stream":true}"#).expect("request");
    for (transport, model_id, canceled) in [
        (Transport::Websocket, "test-model", false),
        (Transport::Sse, "other-model", false),
        (Transport::Sse, "test-model", true),
    ] {
        let mut config = config();
        config.transport = transport;
        let mut dispatches = 0;
        let outcome = run_prepared_sse_attempt_with_diagnostics(
            &minimal_prompt(),
            &config,
            &AttemptModel {
                id: ModelName::new(model_id),
            },
            &request,
            false,
            CacheDiagnostics::Off,
            None,
            &mut |update| {
                if matches!(update, AttemptUpdate::Dispatched(_)) {
                    dispatches += 1;
                }
            },
            &mut || canceled,
            &test_network(),
        );
        assert_eq!(dispatches, 0);
        if canceled {
            assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
        } else {
            assert!(matches!(outcome, AttemptOutcome::Terminal(_)));
        }
    }
}

/// The actual send preserves raw replay syntax and provider-owned request
/// fields; output, usage, identity, and diagnostic capture still use the shared
/// parser.
#[test]
fn prepared_sse_sends_exact_body_and_reuses_terminal_and_capture_policy() {
    const REQUEST: &str = r#"{
 "model":"test-model", "input":[{"type":"reasoning","id":"opaque",
 "encrypted_content":"cipher","summary":[],"extra":1.00}],
 "stream":true,"store":false,"include":["reasoning.encrypted_content"],
 "reasoning":{"effort":"high"},"provider_owned":{"enabled":true}
}"#;
    let listener = TcpListener::bind("127.0.0.1:0").expect("local peer");
    let address = listener.local_addr().expect("peer address");
    listener.set_nonblocking(true).expect("async accept");
    let server = std::thread::spawn(move || {
        let runtime = path_tokio_runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("peer runtime");
        let mut socket = runtime.block_on(async {
            let listener = AsyncTcpListener::from_std(listener).expect("async listener");
            tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("bounded accept")
                .expect("accept request")
                .0
                .into_std()
                .expect("blocking socket")
        });
        socket.set_nonblocking(false).expect("blocking reads");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bounded read");
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("bounded write");
        let (headers, body) = read_http_request(&mut socket);
        assert!(headers.contains("authorization: Bearer credential-canary"));
        assert_eq!(body, REQUEST.as_bytes());
        let event = r#"data: {"type":"response.completed","response":{"id":"response-id","usage":{"input_tokens":11,"output_tokens":2},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}

"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{event}",
            event.len()
        )
        .expect("terminal response");
    });
    let captures = Arc::new(Mutex::new(Vec::new()));
    let sink_captures = captures.clone();
    let sink = Arc::new(
        move |capture: tau_provider::debug_capture_writer::ProviderDebugCapture| {
            if capture.class() != ProviderDebugCaptureClass::ProviderAttemptTiming {
                sink_captures
                    .lock()
                    .expect("capture lock")
                    .push(serde_json::from_slice::<Value>(capture.json()).expect("capture JSON"));
            }
        },
    );
    let mut config = config();
    config.base_url = format!("http://{address}");
    let mut dispatches = 0;
    let (outcome, rows) = collect(|| {
        DebugCapture::with_test_sink_scope(sink, || {
            run_prepared_sse_attempt_with_diagnostics(
                &minimal_prompt(),
                &config,
                &model(),
                &prepared(REQUEST).expect("request"),
                true,
                CacheDiagnostics::Metadata,
                Some(tau_proto::ProviderAttempt::ONE),
                &mut |update| {
                    if matches!(update, AttemptUpdate::Dispatched(_)) {
                        dispatches += 1;
                    }
                },
                &mut || false,
                &test_network(),
            )
        })
    });
    server.join().expect("peer finished");
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("expected shared parser completion");
    };
    assert_eq!(dispatches, 1);
    assert_eq!(success.stop_reason, ProviderStopReason::EndTurn);
    assert_eq!(success.provider_response_id.as_deref(), Some("response-id"));
    assert_eq!(
        success.usage.expect("terminal usage").prompt_sent_tokens,
        11
    );
    assert_eq!(success.output_items.len(), 1);
    assert_eq!(rows[0]["input_item_count"], 1);
    assert_eq!(rows[0]["reasoning_selector"], "high");
    assert_eq!(rows[1]["outcome"], "success");
    let captures = captures.lock().expect("captures");
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0]["body"]["store"], false);
    assert_eq!(captures[0]["body"]["provider_owned"]["enabled"], true);
    assert!(!captures[0].to_string().contains("credential-canary"));
}
