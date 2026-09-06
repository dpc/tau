use tau_provider::cache_diagnostic::CacheDiagnostics;

use super::*;
use crate::cache_diagnostic::CacheAttempt;
use crate::cache_diagnostic::tests::collect;

/// Fixed terminal containing raw counters that canonical normalization clamps.
fn terminal_event() -> Value {
    serde_json::json!({"type": "response.completed", "response": {
        "id": "provider-id-canary", "model": "reported-model-canary",
        "usage": {"input_tokens": 10, "output_tokens": 2,
            "input_tokens_details": {"cached_tokens": 99, "cache_write_tokens": 88}},
        "output": [{"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "payload-canary"}]}]
    }})
}

/// Drive both real transport boundaries against a finite local peer.
/// Cancellation inside the exact request sink exercises the final pre-dispatch
/// check.
fn captured_attempt(
    transport: Transport,
    policy: CacheDiagnostics,
    durable: bool,
    operation: tau_proto::PromptOperation,
    cancel_at_request: bool,
    event: Value,
) -> (AttemptOutcome, Vec<Value>, Vec<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local peer");
    let address = listener.local_addr().expect("peer address");
    let server = if transport == Transport::Sse && cancel_at_request {
        None
    } else {
        Some(std::thread::spawn(move || {
            let mut socket = accept_websocket_peer(&listener);
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("bounded peer read");
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .expect("bounded peer write");
            match transport {
                Transport::Sse => {
                    let _ = read_http_request(&mut socket);
                    let body = format!("data: {event}\n\n");
                    write!(socket, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).expect("SSE terminal");
                }
                Transport::Websocket => {
                    let mut socket = tungstenite::accept(socket).expect("WS upgrade");
                    if cancel_at_request {
                        assert!(socket.read().is_err());
                    } else {
                        assert!(matches!(
                            socket.read().expect("request frame"),
                            Message::Text(_)
                        ));
                        socket
                            .send(Message::Text(event.to_string().into()))
                            .expect("WS terminal");
                        let _ = socket.read();
                    }
                }
            }
        }))
    };
    let exact = Arc::new(Mutex::new(Vec::new()));
    let sink_exact = exact.clone();
    let canceled = Arc::new(AtomicBool::new(false));
    let sink_canceled = canceled.clone();
    let sink = Arc::new(
        move |capture: tau_provider::debug_capture_writer::ProviderDebugCapture| {
            let value = serde_json::from_slice::<Value>(capture.json()).expect("exact JSON");
            sink_exact.lock().expect("exact sink").push(value);
            if cancel_at_request {
                sink_canceled.store(true, Ordering::Relaxed);
            }
        },
    );
    let mut prompt = minimal_prompt();
    prompt.operation = operation;
    let (outcome, rows) = collect(|| {
        DebugCapture::with_test_sink_scope(sink, || {
            run_attempt_with_diagnostics(
                &prompt,
                &AttemptConfig {
                    base_url: format!("http://{address}"),
                    api_key: "credential-canary".into(),
                    max_output_tokens: 0,
                    transport,
                    prompt_cache: None,
                },
                &AttemptModel {
                    id: ModelName::new("test-model"),
                },
                durable,
                policy,
                Some(tau_proto::ProviderAttempt::new(3).expect("nonzero attempt")),
                &mut |_| {},
                &mut || canceled.load(Ordering::Relaxed),
                &test_network(),
            )
        })
    });
    if let Some(server) = server {
        join_websocket_peer(server);
    }
    let exact = exact.lock().expect("exact sink").clone();
    (outcome, rows, exact)
}

/// Both adapters correlate full replay with one actual dispatch, retain raw
/// counters before normalization, and keep payload/provider secrets out of
/// rows.
#[test]
fn cache_diagnostics_transports_correlate_exact_captures_and_raw_usage() {
    for transport in [Transport::Sse, Transport::Websocket] {
        let (outcome, rows, exact) = captured_attempt(
            transport,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
            false,
            terminal_event(),
        );
        let AttemptOutcome::Completed(success) = outcome else {
            panic!("expected completion");
        };
        assert_eq!(
            success.usage.expect("canonical usage").prompt_cached_tokens,
            10
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["record_kind"], "dispatch");
        assert_eq!(rows[0]["wire_dispatch_index"], 1);
        assert_eq!(rows[0]["request_form"], "full");
        assert_eq!(rows[0]["previous_response_present"], false);
        assert_eq!(rows[0]["harness_provider_attempt"], 3);
        assert!(rows[0]["logical_attempt"].is_null());
        assert_eq!(rows[1]["record_kind"], "attempt_end");
        assert_eq!(rows[1]["outcome"], "success");
        assert_eq!(rows[1]["dispatch_count"], 1);
        assert_eq!(rows[1]["reported_usage"]["read_tokens"], 99);
        assert_eq!(rows[1]["reported_usage"]["write_tokens"], 88);
        assert_eq!(exact.len(), 2);
        assert!(exact[0]["wire_dispatch_index"].is_null());
        assert_eq!(exact[1]["wire_dispatch_index"], 1);
        for row in rows.iter().chain(exact.iter()) {
            assert_eq!(row["attempt_id"], rows[0]["attempt_id"]);
        }
        let text = serde_json::to_string(&rows).expect("scalar JSON");
        for canary in [
            "payload-canary",
            "credential-canary",
            "provider-id-canary",
            "reported-model-canary",
            "127.0.0.1",
        ] {
            assert!(!text.contains(canary), "{canary}");
        }
    }
}

/// An exact request is submitted before the last cancellation check on both
/// transports: its identity survives, but no prospective dispatch is invented.
#[test]
fn cache_diagnostics_capture_cancellation_has_zero_actual_dispatches() {
    for transport in [Transport::Sse, Transport::Websocket] {
        let (outcome, rows, exact) = captured_attempt(
            transport,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
            true,
            terminal_event(),
        );
        assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["record_kind"], "attempt_end");
        assert_eq!(rows[0]["dispatch_count"], 0);
        assert_eq!(rows[0]["outcome"], "canceled");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0]["attempt_id"], rows[0]["attempt_id"]);
        assert!(exact[0]["wire_dispatch_index"].is_null());
        assert!(!exact[0].to_string().contains("candidate"));
    }
}

/// Metadata opt-out leaves exact capture intact, nonpersistable work submits
/// neither kind, and local summaries acquire backend-scoped metadata.
#[test]
fn cache_diagnostics_policy_preserves_exact_capture_and_covers_local_summary() {
    for (policy, durable, operation, scalar_count, exact_count) in [
        (
            CacheDiagnostics::Off,
            true,
            tau_proto::PromptOperation::Inference,
            0,
            2,
        ),
        (
            CacheDiagnostics::Metadata,
            false,
            tau_proto::PromptOperation::Inference,
            0,
            0,
        ),
        (
            CacheDiagnostics::Off,
            true,
            tau_proto::PromptOperation::StandaloneCompaction,
            0,
            2,
        ),
        (
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::StandaloneCompaction,
            2,
            2,
        ),
    ] {
        let (outcome, rows, exact) = captured_attempt(
            Transport::Sse,
            policy,
            durable,
            operation,
            false,
            terminal_event(),
        );
        assert!(matches!(outcome, AttemptOutcome::Completed(_)));
        assert_eq!(rows.len(), scalar_count);
        assert_eq!(exact.len(), exact_count);
        if policy == CacheDiagnostics::Off {
            assert!(exact[0]["attempt_id"].is_string());
            assert!(exact[0]["wire_dispatch_index"].is_null());
            assert_eq!(exact[1]["wire_dispatch_index"], 1);
        }
        if operation == tau_proto::PromptOperation::StandaloneCompaction {
            let attempt_id = exact[0]["attempt_id"].clone();
            assert!(attempt_id.is_string());
            assert!(exact.iter().all(|v| v["attempt_id"] == attempt_id));
            assert!(exact[0]["wire_dispatch_index"].is_null());
            assert_eq!(exact[1]["wire_dispatch_index"], 1);
            if policy == CacheDiagnostics::Metadata {
                assert!(
                    rows.iter()
                        .all(|v| v["operation"] == "standalone_compaction")
                );
                assert!(rows.iter().all(|v| v["logical_attempt"].is_null()));
                assert!(rows.iter().all(|v| v["harness_provider_attempt"] == 3));
                assert!(rows.iter().all(|v| v["attempt_id"] == attempt_id));
            }
        }
    }
}

/// Length remains successful and parser validation remains failure; scalar
/// evidence cannot grant new retry/terminal authority or erase raw counters.
#[test]
fn cache_diagnostics_length_and_validation_failure_keep_existing_outcomes() {
    for transport in [Transport::Sse, Transport::Websocket] {
        let mut length = terminal_event();
        length["type"] = "response.incomplete".into();
        length["response"]["incomplete_details"] =
            serde_json::json!({"reason": "max_output_tokens"});
        let (outcome, rows, _) = captured_attempt(
            transport,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
            false,
            length,
        );
        assert!(matches!(
            outcome,
            AttemptOutcome::Completed(AttemptSuccess {
                stop_reason: ProviderStopReason::Length,
                ..
            })
        ));
        assert_eq!(rows[1]["outcome"], "success");
        let mut invalid = terminal_event();
        invalid["response"]["output"] = false.into();
        let (outcome, rows, exact) = captured_attempt(
            transport,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
            false,
            invalid,
        );
        assert!(matches!(outcome, AttemptOutcome::Terminal(_)));
        assert_eq!(rows[1]["outcome"], "error");
        assert_eq!(rows[1]["reported_usage"]["read_tokens"], 99);
        assert!(rows[1]["successful_dispatch_index"].is_null());
        assert_eq!(exact[1]["wire_dispatch_index"], 1);
    }
}

/// Pre-dispatch cancellation, lowering rejection and network-policy rejection
/// produce only an attempt end. Repeated invocations never reuse attempt IDs.
#[test]
fn cache_diagnostics_pre_dispatch_exits_have_identity_without_dispatch() {
    let mut ids = Vec::new();
    for case in 0..3 {
        let mut prompt = minimal_prompt();
        if case == 1 {
            prompt.tools.push(tau_proto::ToolDefinition {
                name: tau_proto::ToolName::new("custom"),
                model_visible_name: None,
                description: None,
                parameters: None,
                tool_type: ToolType::Custom,
                format: None,
            });
        }
        let network = tau_provider::OutboundNetworkPolicy::from_environment(
            BTreeMap::from([("http_proxy".into(), "not a proxy URL".into())]),
            None,
        );
        let exact = Arc::new(Mutex::new(Vec::new()));
        let sink_exact = exact.clone();
        let sink = Arc::new(
            move |capture: tau_provider::debug_capture_writer::ProviderDebugCapture| {
                sink_exact
                    .lock()
                    .expect("exact sink")
                    .push(serde_json::from_slice::<Value>(capture.json()).expect("exact JSON"));
            },
        );
        let (_, rows) = collect(|| {
            DebugCapture::with_test_sink_scope(sink, || {
                run_attempt_with_diagnostics(
                    &prompt,
                    &AttemptConfig {
                        base_url: "http://example.invalid".into(),
                        api_key: String::new(),
                        max_output_tokens: 0,
                        transport: Transport::Sse,
                        prompt_cache: None,
                    },
                    &AttemptModel {
                        id: ModelName::new("model"),
                    },
                    true,
                    CacheDiagnostics::Metadata,
                    None,
                    &mut |_| panic!("no updates"),
                    &mut || case == 0,
                    &network,
                )
            })
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["record_kind"], "attempt_end");
        assert_eq!(rows[0]["dispatch_count"], 0);
        assert_eq!(
            rows[0]["outcome"],
            if case == 0 {
                "canceled"
            } else {
                "pre_dispatch_failure"
            }
        );
        assert!(rows[0]["reported_usage"]["read_tokens"].is_null());
        assert!(rows[0]["harness_provider_attempt"].is_null());
        let exact = exact.lock().expect("exact sink");
        assert_eq!(exact.len(), usize::from(case != 0));
        for capture in exact.iter() {
            assert_eq!(capture["attempt_id"], rows[0]["attempt_id"]);
            assert!(capture["wire_dispatch_index"].is_null());
        }
        assert!(!ids.contains(&rows[0]["attempt_id"]));
        ids.push(rows[0]["attempt_id"].clone());
    }
}

/// Retryable remote errors remain one-dispatch failures without inventing usage
/// or retaining provider error prose in scalar rows.
#[test]
fn cache_diagnostics_remote_failure_keeps_retry_class_content_free() {
    for transport in [Transport::Sse, Transport::Websocket] {
        let event = serde_json::json!({"type": "error",
            "error": {"code": "server_error", "message": "error-prose-canary"}});
        let (outcome, rows, exact) = captured_attempt(
            transport,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
            false,
            event,
        );
        assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["outcome"], "error");
        assert!(rows[1]["retry_class"].is_string());
        assert!(rows[1]["reported_usage"]["input_tokens"].is_null());
        assert!(!rows[1].to_string().contains("error-prose-canary"));
        assert_eq!(exact[1]["wire_dispatch_index"], 1);
    }
}

/// The content-free oversized replacement must retain the same identity and
/// null request index without expanding the existing one-MiB exact bound.
#[test]
fn cache_diagnostics_oversized_exact_request_retains_null_correlation() {
    let mut prompt = minimal_prompt();
    prompt.system_prompt = "x".repeat(1024 * 1024);
    let config = AttemptConfig {
        base_url: "http://example.invalid".into(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    };
    let model = AttemptModel {
        id: ModelName::new("model"),
    };
    let exact = Arc::new(Mutex::new(Vec::new()));
    let sink_exact = exact.clone();
    let mut capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |capture| {
            sink_exact
                .lock()
                .expect("exact sink")
                .push(serde_json::from_slice::<Value>(capture.json()).expect("exact JSON"));
        }),
    );
    let cache = Arc::new(
        CacheAttempt::new(&prompt, true, CacheDiagnostics::Off, None).expect("durable correlation"),
    );
    capture.cache = Some(cache.clone());
    let body = build_request(&prompt, &config, &model).expect("valid request");
    capture.submit_request(&prompt, &config, &model, &body);
    let exact = exact.lock().expect("exact sink");
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0]["attempt_id"], serde_json::json!(cache.id));
    assert!(exact[0]["wire_dispatch_index"].is_null());
    assert!(exact[0].to_string().len() < 1024);
    assert!(!exact[0].to_string().contains(&"x".repeat(100)));
}

/// Credentials overlapping hex IDs or correlation field names must still be
/// scrubbed from payloads without breaking joins to diagnostic rows.
#[test]
fn cache_diagnostics_short_credentials_cannot_rewrite_correlation() {
    for credential in ["a", "attempt", "wire_dispatch"] {
        for oversized in [false, true] {
            let mut prompt = minimal_prompt();
            prompt.system_prompt = if oversized {
                "x".repeat(1024 * 1024)
            } else {
                format!("payload {credential} suffix")
            };
            let config = AttemptConfig {
                base_url: "http://example.invalid".into(),
                api_key: credential.into(),
                max_output_tokens: 0,
                transport: Transport::Sse,
                prompt_cache: None,
            };
            let model = AttemptModel {
                id: ModelName::new("model"),
            };
            let exact = Arc::new(Mutex::new(Vec::new()));
            let sink_exact = exact.clone();
            let mut capture = DebugCapture::with_test_sink(
                true,
                Arc::new(move |capture| {
                    sink_exact
                        .lock()
                        .expect("exact sink")
                        .push(serde_json::from_slice::<Value>(capture.json()).expect("exact JSON"));
                }),
            );
            let cache = Arc::new(
                CacheAttempt::new(&prompt, true, CacheDiagnostics::Off, None)
                    .expect("durable correlation"),
            );
            capture.cache = Some(cache.clone());
            let body = build_request(&prompt, &config, &model).expect("valid request");
            capture.submit_request(&prompt, &config, &model, &body);
            cache.dispatch(&prompt, &config, &model, &body, 0);
            let state = State::default();
            capture.submit_response(
                &prompt,
                &config,
                &model,
                &state,
                ProviderStopReason::EndTurn,
            );
            let exact = exact.lock().expect("exact sink");
            assert_eq!(exact.len(), 2);
            for record in exact.iter() {
                assert_eq!(
                    record["attempt_id"],
                    serde_json::json!(cache.id),
                    "{credential}"
                );
            }
            assert!(exact[0]["wire_dispatch_index"].is_null());
            assert_eq!(exact[1]["wire_dispatch_index"], 1);
            if !oversized {
                let instructions = exact[0]["body"]["instructions"]
                    .as_str()
                    .expect("instructions");
                assert!(!instructions.contains(credential));
                assert!(instructions.contains("[REDACTED]"));
            }
        }
    }
}

/// The typed 30-minute policy projects the approved unsigned-seconds field,
/// never a transport spelling or a second incompatible schema field.
#[test]
fn cache_diagnostics_cache_ttl_uses_approved_seconds_schema() {
    let prompt = minimal_prompt();
    let model = AttemptModel {
        id: ModelName::new("model"),
    };
    for policy in [
        None,
        Some(PromptCachePolicy {
            mode: PromptCacheMode::Implicit,
            ttl: PromptCacheTtl::Minutes30,
        }),
    ] {
        let config = AttemptConfig {
            base_url: "http://example.invalid".into(),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: policy,
        };
        let body = build_request(&prompt, &config, &model).expect("valid request");
        let cache = CacheAttempt::new(&prompt, true, CacheDiagnostics::Metadata, None)
            .expect("durable metadata");
        let (_, rows) = collect(|| cache.dispatch(&prompt, &config, &model, &body, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["cache_ttl_seconds"],
            serde_json::json!(policy.map(|_| 1800_u64))
        );
        assert!(rows[0].get("cache_ttl").is_none());
    }
}
