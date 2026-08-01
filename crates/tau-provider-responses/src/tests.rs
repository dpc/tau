use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::runtime as path_tokio_runtime;

use super::*;

/// Function-call streaming must preserve the exact argument JSON for the next
/// stateless full-transcript replay rather than reserializing it.
#[test]
fn function_call_arguments_keep_raw_spelling() {
    let mut state = State::default();
    apply_event(
        &mut state,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"run","arguments":""}}"#,
    )
    .expect("function call item");
    apply_event(
        &mut state,
        r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{ \"path\""}"#,
    )
    .expect("argument delta");
    apply_event(
        &mut state,
        r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{ \"path\" : \"/tmp\" }"}"#,
    )
    .expect("argument completion");
    let ContextItem::ToolCall(call) = &state.items[0].item else {
        panic!("function call slot");
    };
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{ \"path\" : \"/tmp\" }")
    );
}

/// Completion events without a `[DONE]` marker must still end generic
/// Responses streams because public providers use several terminal dialects.
#[test]
fn completion_event_ends_stream_without_done_sentinel() {
    let mut state = State::default();
    apply_event(
        &mut state,
        r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#,
    )
    .expect("completion event");
    assert!(state.terminal);
    assert_eq!(state.response_id.as_deref(), Some("resp_1"));
}

/// The text-and-Function public contract must reject image-bearing assistant
/// items at parse time, before a forbidden raw sidecar can reach replay.
#[test]
fn image_assistant_item_is_rejected_at_parse_time() {
    let mut state = State::default();
    let error = apply_event(
        &mut state,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_image","image_url":"https://example.test/image"}]}}"#,
    )
    .expect_err("image content must not enter the transcript");
    assert!(matches!(error, Error::UnsupportedOutput));
    assert!(matches!(
        state.items.first().map(|slot| &slot.item),
        Some(ContextItem::UnknownProviderItem(item)) if item.raw_json.is_none()
    ));
}

/// A public Responses attempt must post to `/responses`, parse semantic SSE
/// completion without a sentinel, and identify its backend as HTTP/SSE.
#[test]
fn http_sse_attempt_posts_responses_and_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let (head, body) = read_http_request(&mut socket);
        assert!(head.starts_with("POST /responses HTTP/1.1"));
        let request: serde_json::Value = serde_json::from_slice(&body).expect("request JSON");
        assert_eq!(request["stream"], true);
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("store").is_none());
        assert_eq!(
            request["input"][0]["content"][0]["text"],
            "replayed user text"
        );
        assert_eq!(request["input"][1]["id"], "msg_1");
        assert_eq!(request["input"][1]["status"], "completed");
        assert_eq!(request["input"][2]["id"], "fc_1");
        assert_eq!(request["input"][2]["call_id"], "call_1");
        assert_eq!(request["input"][2]["name"], "run");
        assert_eq!(request["input"][2]["arguments"], "{ \"path\" : \"/tmp\" }");
        assert_eq!(request["input"][3]["type"], "function_call_output");
        assert_eq!(request["input"][3]["output"], "tool result");
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n";
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let outcome = run_attempt(
        &prompt_with_replayed_user_text(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
    );
    server.join().expect("join test server");
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("Responses SSE attempt must complete");
    };
    assert_eq!(success.provider_response_id.as_deref(), Some("resp_1"));
}

fn read_http_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).expect("read request");
        assert_ne!(read, 0, "request must not end before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).expect("header UTF-8");
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .expect("content length")
        .parse::<usize>()
        .expect("numeric content length");
    while bytes.len().saturating_sub(header_end) < length {
        let read = socket.read(&mut chunk).expect("read request body");
        assert_ne!(read, 0, "request must not end before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    (head, bytes[header_end..header_end + length].to_vec())
}

/// A stalled response header must observe cancellation after its peer has
/// received the request and close that peer instead of retaining the attempt.
///
/// The attempt runner signals cancellation only after the server has read
/// request bytes, which prevents cancellation from racing an unaccepted
/// connection. Bounded reports then verify that dropping the canceled request
/// promptly releases the stalled transport without timing-based teardown.
#[test]
fn stalled_header_observes_cancellation() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let mut request = [0_u8; 1024];
        assert_ne!(socket.read(&mut request).expect("read request"), 0);
        accepted_tx.send(()).expect("report accepted request");
        loop {
            match socket.read(&mut request) {
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
                Err(error) => panic!("canceled request was not closed promptly: {error}"),
            }
        }
        dropped_tx.send(()).expect("report dropped request");
    });
    let canceled = Arc::new(AtomicBool::new(false));
    let attempt_canceled = Arc::clone(&canceled);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let attempt = std::thread::spawn(move || {
        let outcome = run_attempt(
            &minimal_prompt(),
            &AttemptConfig {
                base_url: format!("http://{address}"),
                api_key: String::new(),
                max_output_tokens: 0,
            },
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
            &mut |_| {},
            &mut || attempt_canceled.load(Ordering::SeqCst),
            &test_network(),
        );
        result_tx.send(outcome).expect("report attempt outcome");
    });
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("request did not reach local peer");
    canceled.store(true, Ordering::SeqCst);
    assert!(matches!(
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("canceled request remained blocked"),
        AttemptOutcome::Canceled { .. }
    ));
    dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("canceled request retained its TCP connection");
    attempt.join().expect("join attempt");
    server.join().expect("join server");
}

/// A visible HTTP-proxy 407 must become the shared typed authentication retry,
/// rather than a generic public Responses HTTP failure.
#[test]
fn proxy_407_is_typed_as_auth_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let address = listener.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept proxy request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).expect("read proxy request");
        socket
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .expect("write proxy rejection");
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(
        BTreeMap::from([("http_proxy".to_owned(), format!("http://{address}"))]),
        None,
    );
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: "http://provider.test/v1".to_owned(),
            api_key: String::new(),
            max_output_tokens: 0,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &network,
    );
    proxy.join().expect("join proxy");
    let AttemptOutcome::Retryable { decision, .. } = outcome else {
        panic!("proxy rejection must retry as typed authentication failure");
    };
    assert_eq!(decision.class, tau_provider::retry_policy::RetryClass::Auth);
}

/// Error bodies must stop at the documented 64 KiB cap before diagnostics or
/// retry classification can retain an arbitrarily large provider response.
#[test]
fn error_body_read_is_capped() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind error server");
    let address = listener.local_addr().expect("error server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept error request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).expect("read error request");
        let body = vec![b'x'; MAX_HTTP_ERROR_BODY_BYTES as usize + 1];
        write!(
            socket,
            "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .expect("write error headers");
        socket.write_all(&body).expect("write error body");
    });
    let network = test_network();
    let runtime = path_tokio_runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let body = runtime.block_on(async {
        let url = format!("http://{address}/responses");
        let response = network
            .client_for(&url)
            .expect("client")
            .post(&url)
            .send()
            .await
            .expect("response");
        read_capped_error_body(response, &url, &mut || false, &network)
            .await
            .expect("capped body")
    });
    server.join().expect("join error server");
    assert_eq!(body.len(), MAX_HTTP_ERROR_BODY_BYTES as usize);
}

fn minimal_prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "responses-test".parse().expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        session_id: "session-test".parse().expect("session id"),
        system_prompt: "test system".to_owned(),
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
        operation: tau_proto::PromptOperation::Inference,
    }
}

fn prompt_with_replayed_user_text() -> tau_proto::AgentPromptCreated {
    let mut prompt = minimal_prompt();
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text {
                        text: "replayed user text".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            },
        ));
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: Some("old-response".to_owned()),
                backend: None,
                output_items: vec![
                    ContextItem::Message(MessageItem {
                        role: ContextRole::Assistant,
                        content: vec![ContentPart::Text {
                            text: "assistant replay".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: Some(
                            r#"{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"assistant replay"}]}"#.to_owned(),
                        ),
                    }),
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: tau_proto::ToolCallId::new("call_1"),
                        name: tau_proto::ToolName::try_new("run".to_owned())
                            .expect("tool name"),
                        tool_type: ToolType::Function,
                        arguments: tau_proto::json_to_cbor(&serde_json::json!({"path": "/tmp"})),
                        raw_arguments_json: Some("{ \"path\" : \"/tmp\" }".to_owned()),
                        responses_envelope: Some(ResponsesToolCallEnvelope {
                            item_id: Some("fc_1".to_owned()),
                            status: Some("completed".to_owned()),
                            extra_fields: None,
                        }),
                    }),
                ],
                usage: None,
            },
        ));
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    call_id: tau_proto::ToolCallId::new("call_1"),
                    tool_type: ToolType::Function,
                    status: ToolResultStatus::Success,
                    output: tau_proto::ToolResponse {
                        raw: tau_proto::CborValue::Text("tool result".to_owned()),
                        headers: Vec::new(),
                        body: "tool result".to_owned(),
                    },
                    provider_content: Vec::new(),
                }],
            },
        ));
    prompt
}

fn test_network() -> tau_provider::OutboundNetworkPolicy {
    tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None)
}
