//! Approved Grok partial-limit policy through the actual shared SSE attempt.

use std::io::{Read as _, Write as _};
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use tau_provider::cache_diagnostic::CacheDiagnostics;
use tau_provider_responses::{AttemptOutcome, run_prepared_sse_attempt_with_diagnostics};
use tokio::net::TcpListener;
use tokio::runtime::Builder;

use super::*;

/// Run one synthetic terminal without polling or unbounded fake-server waits.
fn attempt(event: Value) -> AttemptOutcome {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("listen");
    let address = listener.local_addr().expect("address");
    listener.set_nonblocking(true).expect("nonblocking");
    let server = std::thread::spawn(move || {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut socket = runtime.block_on(async {
            let listener = TcpListener::from_std(listener).expect("async listener");
            tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("bounded accept")
                .expect("accept")
                .0
                .into_std()
                .expect("socket")
        });
        socket.set_nonblocking(false).expect("blocking");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read deadline");
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write deadline");
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).expect("header");
            headers.extend(byte);
            assert!(headers.len() < 8192);
        }
        let headers = String::from_utf8(headers).expect("UTF-8");
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|value| value.parse().expect("length"))
            })
            .expect("content length");
        let mut request = vec![0; length];
        socket.read_exact(&mut request).expect("request");
        let event = format!("data: {event}\n\n");
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{event}", event.len())
            .expect("reply");
    });
    let prompt = prompt();
    let model = model();
    let request = Request::lower(&prompt, &model, 0, &[], false).expect("request");
    let config = AttemptConfig {
        base_url: format!("http://{address}"),
        api_key: "synthetic".into(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    };
    let policy = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let mut dispatches = 0;
    let outcome = run_prepared_sse_attempt_with_diagnostics(
        &prompt,
        &config,
        &model,
        request.prepared(),
        false,
        CacheDiagnostics::Metadata,
        None,
        &mut |update| {
            if matches!(update, tau_provider_responses::AttemptUpdate::Dispatched(_)) {
                dispatches += 1;
            }
        },
        &mut || false,
        &policy,
    );
    server.join().expect("server");
    assert_eq!(dispatches, 1);
    outcome
}

/// Both approved limits retain prose/accounting and strip even valid tool
/// calls.
#[test]
fn grok_limits_retain_prose_usage_identity_without_retry_or_tools() {
    for reason in ["max_prompt_tokens", "max_time_limit"] {
        let outcome = attempt(json!({
            "type":"response.incomplete",
            "response":{
                "id":"partial-id","incomplete_details":{"reason":reason},
                "usage":{"input_tokens":11,"output_tokens":7},
                "output":[
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial prose"}]},
                    {"type":"function_call","id":"fc_1","call_id":"call_1","name":"run","arguments":"{}"},
                    {"type":"function_call","id":"fc_2","call_id":"call_2","name":"run","arguments":"{\"truncated\"","status":"in_progress"},
                    {"type":"reasoning","id":"rs_1","encrypted_content":"sealed","summary":[]}
                ]
            }
        }));
        let AttemptOutcome::Terminal(failure) = outcome else {
            panic!("limit must terminalize without retry");
        };
        assert_eq!(failure.stop_reason, ProviderStopReason::Error);
        assert_eq!(failure.failure_kind, None);
        assert_eq!(failure.provider_response_id.as_deref(), Some("partial-id"));
        let usage = failure.usage.expect("terminal usage");
        assert_eq!(usage.prompt_sent_tokens, 11);
        assert_eq!(usage.response_received_tokens, 7);
        assert!(
            matches!(failure.output_items.as_slice(), [ContextItem::Message(message)]
            if message.content == vec![ContentPart::Text { text: "partial prose".into() }])
        );
    }
}

/// Empty limit output retains accounting; unknown reasons keep generic policy.
#[test]
fn grok_limits_allow_no_prose_but_do_not_capture_unknown_reasons() {
    for (reason, terminal) in [("max_time_limit", true), ("max_time_limit_extra", false)] {
        let outcome = attempt(json!({
            "type":"response.incomplete","response":{
                "incomplete_details":{"reason":reason},"output":[],
                "usage":{"input_tokens":3,"output_tokens":0}
            }
        }));
        if terminal {
            let AttemptOutcome::Terminal(failure) = outcome else {
                panic!("terminal limit")
            };
            assert!(failure.output_items.is_empty());
            assert_eq!(failure.usage.expect("usage").prompt_sent_tokens, 3);
            assert_eq!(failure.stop_reason, ProviderStopReason::Error);
        } else {
            assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
        }
    }
}
