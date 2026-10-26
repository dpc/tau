use super::*;

/// Exercise the real HTTP error path, including failed WebSocket upgrades,
/// without an external provider or transport fallback.
fn rejected_http(transport: Transport, status: u16, body: String, headers: &str) -> AttemptOutcome {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rejection server");
    let address = listener.local_addr().expect("rejection server address");
    let headers = headers.to_owned();
    let server = std::thread::spawn(move || {
        let mut socket = accept_websocket_peer(&listener);
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("write timeout");
        if transport == Transport::Sse {
            let _ = read_http_request(&mut socket);
        } else {
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).expect("read upgrade headers");
                headers.push(byte[0]);
                assert!(headers.len() < 16 * 1024, "bounded upgrade request");
            }
            assert!(headers.starts_with(b"GET /responses "));
        }
        write!(
            socket,
            "HTTP/1.1 {status} Rejected\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
            body.len()
        ).expect("write rejection");
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    join_websocket_peer(server);
    outcome
}

/// Reviewed structured evidence must survive conflicting numeric statuses on
/// HTTP errors, SSE events, and WebSocket events before any semantic output.
#[test]
fn structured_transients_survive_deterministic_status_fallback() {
    for status in [400, 404, 409, 413, 422] {
        for (code, class) in [
            ("rate_limit_exceeded", RetryClass::Throttle),
            ("invalid_api_key", RetryClass::Auth),
            ("usage_limit_reached", RetryClass::UsageWindow),
            ("insufficient_quota", RetryClass::Account),
            ("server_error", RetryClass::Overload),
        ] {
            let event = serde_json::json!({"type":"error", "status":status, "error":{"code":code}})
                .to_string();
            for outcome in [
                rejected_http(Transport::Sse, status, event.clone(), ""),
                run_sse_terminal_event(&event),
                run_websocket_message(Message::Text(event.into()), &mut || false),
            ] {
                let AttemptOutcome::Retryable {
                    decision, progress, ..
                } = outcome
                else {
                    panic!("{status}/{code} must retry");
                };
                assert_eq!(decision.class, class);
                assert!(progress.output_items.is_empty());
            }
        }
    }
}

/// Numeric unknown-400 rejection and explicit terminal identifiers retain their
/// authority even when headers offer a delay or a status normally retries.
#[test]
fn explicit_and_unknown_request_rejections_remain_terminal() {
    for (status, code, kind) in [
        (
            400,
            "unknown",
            tau_proto::ProviderFailureKind::RequestRejected,
        ),
        (
            429,
            "invalid_request_error",
            tau_proto::ProviderFailureKind::RequestRejected,
        ),
        (
            503,
            "model_not_found",
            tau_proto::ProviderFailureKind::RequestRejected,
        ),
        (
            429,
            "context_length_exceeded",
            tau_proto::ProviderFailureKind::ContextWindowExceeded,
        ),
    ] {
        let event =
            serde_json::json!({"type":"error", "status":status, "error":{"code":code}}).to_string();
        let body = serde_json::json!({"error":{"code":code}}).to_string();
        for outcome in [
            rejected_http(
                Transport::Sse,
                status,
                body.clone(),
                "Retry-After: 3600\r\n",
            ),
            rejected_http(Transport::Websocket, status, body, "Retry-After: 3600\r\n"),
            run_sse_terminal_event(&event),
            run_websocket_message(Message::Text(event.into()), &mut || false),
        ] {
            let AttemptOutcome::Terminal(failure) = outcome else {
                panic!("{status}/{code} must terminalize");
            };
            assert_eq!(failure.failure_kind, Some(kind));
        }
    }
}

/// Preserve standard Retry-After on both response-header error paths, without
/// granting arbitrary reset headers or body prose scheduling authority.
#[test]
fn http_and_upgrade_retry_after_reaches_retry_decision() {
    for transport in [Transport::Sse, Transport::Websocket] {
        for (header, expected) in [
            ("Retry-After: 3600\r\n", Some(Duration::from_secs(3600))),
            ("Retry-After: 0\r\n", Some(Duration::ZERO)),
            ("Retry-After: -1\r\n", None),
            ("Retry-After: invalid\r\n", None),
            ("X-RateLimit-Reset: 3600\r\n", None),
        ] {
            for (code, class) in [
                ("rate_limit_exceeded", RetryClass::Throttle),
                ("usage_limit_reached", RetryClass::UsageWindow),
            ] {
                let body = serde_json::json!({"error":{"code":code,"message":"retry after 7200","reset":7200}}).to_string();
                let AttemptOutcome::Retryable { decision, .. } =
                    rejected_http(transport, 400, body, header)
                else {
                    panic!("structured {code} must retry after rejected HTTP/upgrade");
                };
                assert_eq!(decision.class, class);
                assert_eq!(decision.retry_after, expected);
            }
        }
        let AttemptOutcome::Retryable { decision, .. } = rejected_http(
            transport,
            429,
            r#"{"error":{"code":"rate_limit_exceeded"}}"#.to_owned(),
            "Retry-After: Thu, 01 Jan 1970 00:00:00 GMT\r\n",
        ) else {
            panic!("HTTP date retry hint must remain retryable");
        };
        assert_eq!(decision.retry_after, Some(Duration::ZERO));
    }
}
