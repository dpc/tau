use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use tokio::runtime as path_tokio_runtime;
use tungstenite::Message;
use tungstenite::handshake::server::Request as WebSocketRequest;

use super::*;

/// Public Responses usage preserves OpenAI cache reads and writes as separate
/// normalized classes without exposing a cache key or prompt content.
#[test]
fn usage_parses_distinct_cache_read_and_write_classes() {
    let usage = parse_usage(Some(&serde_json::json!({
        "input_tokens": 100,
        "output_tokens": 5,
        "input_tokens_details": {
            "cached_tokens": 70,
            "cache_write_tokens": 20
        }
    })))
    .expect("Responses usage");

    assert_eq!(usage.prompt_cached_tokens, 70);
    let cache = usage.cache.expect("cache usage");
    assert_eq!(cache.read_tokens, Some(70));
    assert_eq!(cache.write_tokens, Some(20));
    assert_eq!(cache.avoided_prefill_tokens, Some(70));
}

/// DeepSeek-style reasoning must stream as full thinking immediately, then
/// become a paired display item and durable opaque replay item at completion
/// without displacing the ordinary assistant response.
#[test]
fn plain_reasoning_streams_displays_and_materializes_for_replay() {
    let mut state = State::default();
    state.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","status":"in_progress","summary":[],"content":[]}}"#,
    )
    .expect("reasoning added");
    state.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"think "}"#,
    )
    .expect("reasoning delta");

    let progress = state.progress();
    assert!(progress.has_timed_semantic_output);
    assert!(matches!(
        progress.output_items.as_slice(),
        [AttemptOutputItem {
            output_index: 0,
            item: ContextItem::ReasoningText(ReasoningTextItem {
                kind: ReasoningTextKind::Full,
                text,
            }),
        }] if text == "think "
    ));

    state.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":1,"delta":"carefully"}"#,
    )
    .expect("second reasoning delta");
    state.apply_event(r#"{"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"think "}"#,
    )
    .expect("first reasoning text done");
    state.apply_event(r#"{"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":1,"text":"carefully"}"#,
    )
    .expect("second reasoning text done");
    state.apply_event(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"think "},{"type":"reasoning_text","text":"carefully"}],"provider_number":1.2300,"provider_future":true}}"#,
    )
    .expect("reasoning item done");
    state.apply_event(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","role":"assistant","content":[]}}"#,
    )
    .expect("message added");
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":1,"delta":"answer"}"#)
        .expect("message delta");
    state.apply_event(r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#,
    )
    .expect("message done");

    let output = state.output_items();
    assert_eq!(output.len(), 3);
    assert!(matches!(
        &output[0],
        ContextItem::ReasoningText(ReasoningTextItem {
            kind: ReasoningTextKind::Full,
            text,
        }) if text == "think carefully"
    ));
    let ContextItem::Reasoning(reasoning) = &output[1] else {
        panic!("durable reasoning item");
    };
    assert_eq!(
        reasoning.raw_json.as_deref(),
        Some(
            r#"{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"think "},{"type":"reasoning_text","text":"carefully"}],"provider_number":1.2300,"provider_future":true}"#
        )
    );
    let raw_reasoning: serde_json::Value = serde_json::from_str(
        reasoning
            .raw_json
            .as_deref()
            .expect("reasoning replay sidecar"),
    )
    .expect("reasoning replay JSON");
    assert_eq!(
        raw_reasoning,
        serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "status": "completed",
            "summary": [],
            "content": [
                {"type": "reasoning_text", "text": "think "},
                {"type": "reasoning_text", "text": "carefully"},
            ],
            "provider_number": 1.23,
            "provider_future": true,
        })
    );
    assert!(matches!(
        &output[2],
        ContextItem::Message(message)
            if message.content == vec![ContentPart::Text {
                text: "answer".to_owned(),
            }]
    ));
}

/// A terminal response output array is authoritative when a provider omits
/// incremental item events, including for plain reasoning followed by text.
#[test]
fn terminal_output_fallback_materializes_plain_reasoning_and_text() {
    let mut state = State::default();
    state.apply_event(r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"fallback thought"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback answer"}]}]}}"#,
    )
    .expect("terminal output");

    assert!(state.terminal);
    assert!(!state.has_incomplete_reasoning());
    let output = state.output_items();
    assert_eq!(output.len(), 3);
    assert!(matches!(
        &output[0],
        ContextItem::ReasoningText(reasoning)
            if reasoning.kind == ReasoningTextKind::Full
                && reasoning.text == "fallback thought"
    ));
    assert!(matches!(&output[1], ContextItem::Reasoning(_)));
    assert!(matches!(
        &output[2],
        ContextItem::Message(message)
            if message.content == vec![ContentPart::Text {
                text: "fallback answer".to_owned(),
            }]
    ));
}

/// Full-transcript lowering must skip the display-only companion and prefer
/// the durable reasoning item's Responses replay sidecar.
#[test]
fn plain_reasoning_replay_prefers_sidecar_and_skips_display_item() {
    let display = ContextItem::ReasoningText(ReasoningTextItem {
        kind: ReasoningTextKind::Full,
        text: "display text".to_owned(),
    });
    assert!(lower_item(&display).expect("display lowering").is_none());

    let replay = ContextItem::Reasoning(tau_proto::OpaqueProviderItem {
        value: tau_proto::json_to_cbor(&serde_json::json!({
            "type": "reasoning",
            "content": [{"type": "reasoning_text", "text": "stale"}],
        })),
        raw_json: Some(
            r#"{"type":"reasoning","id":"rs_raw","summary":[],"content":[{"type":"reasoning_text","text":"replay me"}],"provider_number":1.2300,"provider_future":17}"#
                .to_owned(),
        ),
    });
    let lowered = lower_item(&replay)
        .expect("reasoning lowering")
        .expect("reasoning input");
    let replay_body = serde_json::to_string(&lowered).expect("serialize replay authority");
    assert!(
        replay_body.contains(r#""provider_number":1.2300"#),
        "raw replay must preserve numeric spelling: {replay_body}"
    );
    let lowered = serde_json::to_value(lowered).expect("serialize reasoning input");
    assert_eq!(lowered["id"], "rs_raw");
    assert_eq!(lowered["content"][0]["text"], "replay me");
    assert_eq!(lowered["provider_future"], 17);
}

/// Public Responses must keep rejecting every reasoning shape outside plain
/// `reasoning_text`, as well as unsupported output item families.
#[test]
fn malformed_or_unsupported_reasoning_output_is_rejected() {
    let unsupported = [
        r#"{"type":"reasoning","summary":[]}"#,
        r#"{"type":"reasoning","summary":[],"content":[]}"#,
        r#"{"type":"reasoning","summary":[],"content":"thought"}"#,
        r#"{"type":"reasoning","summary":[],"content":[{"type":"summary_text","text":"summary"}]}"#,
        r#"{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":1}]}"#,
        r#"{"type":"reasoning","summary":[],"encrypted_content":"SEALED","content":[{"type":"reasoning_text","text":"thought"}]}"#,
        r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],"content":[{"type":"reasoning_text","text":"thought"}]}"#,
        r#"{"type":"custom_tool_call","call_id":"call_1","name":"shell","input":"pwd"}"#,
        r#"{"type":"web_search_call","id":"search_1"}"#,
        r#"{"type":"future_output","value":"unknown"}"#,
    ];

    for item in unsupported {
        let mut state = State::default();
        let event =
            format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{item}}}"#);
        assert!(
            matches!(state.apply_event(&event), Err(Error::UnsupportedOutput)),
            "unexpectedly accepted {item}"
        );
    }
}

/// Reasoning deltas require an added plain-reasoning slot, and a stream cannot
/// terminalize while that slot lacks a validated completed item.
#[test]
fn incomplete_or_unscoped_reasoning_stream_is_rejected() {
    let mut state = State::default();
    assert!(matches!(
        state.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_orphan","output_index":0,"content_index":0,"delta":"orphan"}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));

    let mut state = State::default();
    state.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","summary":[],"content":[]}}"#,
    )
    .expect("reasoning added");
    state.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_unfinished","output_index":0,"content_index":0,"delta":"unfinished"}"#,
    )
    .expect("reasoning delta");
    assert!(state.has_incomplete_reasoning());
    assert!(state.output_items().is_empty());
}

/// The finite attempt must reject a stream that displays reasoning but reaches
/// its sentinel without a validated durable reasoning item.
#[test]
fn incomplete_reasoning_attempt_rejects_terminal_sentinel() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"summary\":[],\"content\":[]}}\n\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"item_id\":\"rs_unfinished\",\"output_index\":0,\"content_index\":0,\"delta\":\"unfinished\"}\n\n",
            "data: [DONE]\n\n",
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    server.join().expect("join SSE server");
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("incomplete reasoning must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert!(failure.progress.has_timed_semantic_output);
}

/// Reasoning event indices are required exact `u32` values so malformed
/// provider events cannot alias an existing output slot.
#[test]
fn malformed_reasoning_output_indices_are_rejected() {
    for output_index in ["", r#""0""#, "4294967296"] {
        let mut state = State::default();
        let member = if output_index.is_empty() {
            String::new()
        } else {
            format!(r#","output_index":{output_index}"#)
        };
        let event = format!(
            r#"{{"type":"response.output_item.added"{member},"item":{{"type":"reasoning","summary":[],"content":[]}}}}"#
        );
        assert!(
            matches!(state.apply_event(&event), Err(Error::UnsupportedOutput)),
            "unexpectedly accepted output_index {output_index}"
        );
    }
}

/// Once a reasoning slot streams display text, only one matching reasoning
/// completion may create its durable authority.
#[test]
fn reasoning_completion_rejects_cross_type_and_repeated_transitions() {
    for done_item in [
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}"#,
        r#"{"type":"function_call","call_id":"call_1","name":"run","arguments":"{}"}"#,
    ] {
        let mut state = reasoning_delta_state();
        let event = format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{done_item}}}"#
        );
        assert!(matches!(
            state.apply_event(&event),
            Err(Error::UnsupportedOutput)
        ));
    }

    let mut state = reasoning_delta_state();
    let done = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_test","summary":[],"content":[{"type":"reasoning_text","text":"thought"}]}}"#;
    state.apply_event(done).expect("first reasoning completion");
    assert!(matches!(
        state.apply_event(done),
        Err(Error::UnsupportedOutput)
    ));

    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"run","arguments":"{}"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"run","arguments":"{}"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"answer"}"#,
    ] {
        let mut state = completed_reasoning_state();
        assert!(
            matches!(state.apply_event(event), Err(Error::UnsupportedOutput)),
            "completed reasoning accepted conflicting event {event}"
        );
    }

    let mut pending = reasoning_delta_state();
    assert!(matches!(
        pending.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","summary":[],"content":[]}}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));
}

/// Every streaming representation of one reasoning slot must agree on its
/// provider identity and accumulated full text.
#[test]
fn reasoning_stream_rejects_identity_or_text_contradictions() {
    for event in [
        r#"{"type":"response.reasoning_text.delta","output_index":0,"content_index":0,"delta":"missing id"}"#,
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_test","output_index":0,"delta":"missing content index"}"#,
    ] {
        let mut state = reasoning_delta_state();
        let before = state.items[0].reasoning_parts.clone();
        assert!(matches!(
            state.apply_event(event),
            Err(Error::UnsupportedOutput)
        ));
        assert_eq!(state.items[0].reasoning_parts, before);
    }

    let mut done_mismatch = reasoning_delta_state();
    assert!(matches!(
        done_mismatch.apply_event(r#"{"type":"response.reasoning_text.done","item_id":"rs_test","output_index":0,"content_index":0,"text":"different"}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));

    let mut item_text_mismatch = reasoning_delta_state();
    assert!(matches!(
        item_text_mismatch.apply_event(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_test","summary":[],"content":[{"type":"reasoning_text","text":"different"}]}}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));

    let mut id_mismatch = State::default();
    id_mismatch.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_added","summary":[],"content":[]}}"#,
    )
    .expect("reasoning added");
    assert!(matches!(
        id_mismatch.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_other","output_index":0,"content_index":0,"delta":"foreign"}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));
    assert!(id_mismatch.items[0].reasoning_parts.is_empty());
    assert_eq!(
        id_mismatch.items[0].reasoning_item_id,
        Some(ReasoningItemId("rs_added".to_owned()))
    );
    assert!(matches!(
        id_mismatch.apply_event(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_done","summary":[],"content":[{"type":"reasoning_text","text":"thought"}]}}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));
}

/// Reasoning parts may advance only in append-only content-index order, and a
/// terminalized part rejects every later delta or duplicate done event.
#[test]
fn reasoning_parts_enforce_append_only_streaming_and_single_completion() {
    let mut state = State::default();
    state
        .apply_event(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_parts","summary":[],"content":[]}}"#,
        )
        .expect("reasoning added");
    for event in [
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_parts","output_index":0,"content_index":0,"delta":"a"}"#,
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_parts","output_index":0,"content_index":1,"delta":"b"}"#,
    ] {
        state.apply_event(event).expect("ordered reasoning delta");
    }
    let before = state.progress();
    assert!(matches!(
        &before.output_items[0].item,
        ContextItem::ReasoningText(item) if item.text == "ab"
    ));
    assert!(matches!(
        state.apply_event(
            r#"{"type":"response.reasoning_text.delta","item_id":"rs_parts","output_index":0,"content_index":0,"delta":"c"}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));
    assert!(matches!(
        &state.progress().output_items[0].item,
        ContextItem::ReasoningText(item) if item.text == "ab"
    ));

    let mut state = reasoning_delta_state();
    let done = r#"{"type":"response.reasoning_text.done","item_id":"rs_test","output_index":0,"content_index":0,"text":"thought"}"#;
    state.apply_event(done).expect("reasoning part done");
    assert!(matches!(
        state.apply_event(done),
        Err(Error::UnsupportedOutput)
    ));
    assert!(matches!(
        state.apply_event(
            r#"{"type":"response.reasoning_text.delta","item_id":"rs_test","output_index":0,"content_index":0,"delta":"again"}"#,
        ),
        Err(Error::UnsupportedOutput)
    ));
}

/// A terminal output array is authoritative, but it may not contradict
/// streamed reasoning by silently dropping or changing its durable item.
#[test]
fn terminal_output_rejects_streamed_reasoning_disagreement() {
    for mut state in [reasoning_delta_state(), completed_reasoning_state()] {
        assert!(matches!(
            state.apply_event(r#"{"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}]}}"#,
            ),
            Err(Error::UnsupportedOutput)
        ));
    }

    for reasoning in [
        r#"{"type":"reasoning","id":"different","summary":[],"content":[{"type":"reasoning_text","text":"thought"}]}"#,
        r#"{"type":"reasoning","id":"rs_test","summary":[],"content":[{"type":"reasoning_text","text":"different"}]}"#,
        r#"{"type":"reasoning","id":"rs_test","summary":[],"content":[{"type":"reasoning_text","text":"thought"}],"provider_future":true}"#,
    ] {
        let mut state = completed_reasoning_state();
        let event =
            format!(r#"{{"type":"response.completed","response":{{"output":[{reasoning}]}}}}"#);
        assert!(
            matches!(state.apply_event(&event), Err(Error::UnsupportedOutput)),
            "terminal reasoning replaced authority with {reasoning}"
        );
        assert!(!state.terminal);
        assert!(state.response_id.is_none());
        assert!(matches!(
            state.output_items().as_slice(),
            [ContextItem::ReasoningText(_), ContextItem::Reasoning(_)]
        ));
    }
}

/// Replay validation must reject malformed raw sidecars and malformed legacy
/// CBOR fallbacks instead of forwarding arbitrary opaque provider items.
#[test]
fn invalid_reasoning_replay_authorities_are_rejected() {
    let invalid = [
        tau_proto::OpaqueProviderItem {
            value: tau_proto::CborValue::Null,
            raw_json: Some("{".to_owned()),
        },
        tau_proto::OpaqueProviderItem {
            value: tau_proto::CborValue::Null,
            raw_json: Some(
                r#"{"type":"reasoning","encrypted_content":"SEALED","content":[{"type":"reasoning_text","text":"thought"}]}"#
                    .to_owned(),
            ),
        },
        tau_proto::OpaqueProviderItem {
            value: tau_proto::json_to_cbor(&serde_json::json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "summary"}],
            })),
            raw_json: None,
        },
    ];
    for item in invalid {
        assert!(matches!(
            lower_item(&ContextItem::Reasoning(item)),
            Err(Error::UnsupportedOutput)
        ));
    }
}

fn reasoning_delta_state() -> State {
    let mut state = State::default();
    state.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_test","summary":[],"content":[]}}"#,
    )
    .expect("reasoning added");
    state.apply_event(r#"{"type":"response.reasoning_text.delta","item_id":"rs_test","output_index":0,"content_index":0,"delta":"thought"}"#,
    )
    .expect("reasoning delta");
    state
}

fn completed_reasoning_state() -> State {
    let mut state = reasoning_delta_state();
    state.apply_event(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_test","summary":[],"content":[{"type":"reasoning_text","text":"thought"}]}}"#,
    )
    .expect("reasoning completed");
    state
}

/// Function-call streaming must preserve the exact argument JSON for the next
/// stateless full-transcript replay rather than reserializing it.
#[test]
fn function_call_arguments_keep_raw_spelling() {
    let mut state = State::default();
    state.apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"run","arguments":""}}"#,
    )
    .expect("function call item");
    state.apply_event(r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{ \"path\""}"#,
    )
    .expect("argument delta");
    state.apply_event(r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{ \"path\" : \"/tmp\" }"}"#,
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
    state.apply_event(r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#,
    )
    .expect("completion event");
    assert!(state.terminal);
    assert_eq!(state.response_id.as_deref(), Some("resp_1"));
}

/// The text-only assistant contract must reject image- and file-bearing items
/// at parse time, before a forbidden raw sidecar can reach replay.
#[test]
fn image_or_file_assistant_item_is_rejected_at_parse_time() {
    for part in [
        r#"{"type":"output_image","image_url":"https://example.test/image"}"#,
        r#"{"type":"output_file","file_id":"file_1"}"#,
    ] {
        let mut state = State::default();
        let event = format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"message","role":"assistant","content":[{part}]}}}}"#
        );
        let error = state
            .apply_event(&event)
            .expect_err("non-text content must be rejected");
        assert!(matches!(error, Error::UnsupportedOutput));
        assert!(state.items.is_empty());
    }
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
        assert_eq!(request["reasoning"]["effort"], "none");
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
            transport: Transport::Sse,
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

/// Public Responses requests must always transmit the harness-effective effort:
/// `off` uses the API's explicit `none` spelling and `max` remains selectable.
#[test]
fn request_lowers_off_and_max_reasoning_efforts() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
    };
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };

    for (effort, expected) in [
        (tau_proto::Effort::Off, "none"),
        (tau_proto::Effort::Max, "max"),
    ] {
        let mut prompt = minimal_prompt();
        prompt.model_params.effort = effort;
        let request = build_request(&prompt, &config, &model).expect("request");
        let request = serde_json::to_value(request).expect("serialize request");

        assert_eq!(request["reasoning"]["effort"], expected);
    }
}

/// Post-upgrade provider errors must terminate known auth/request/context
/// failures and retain retry classes only for service-side failures.
#[test]
fn websocket_provider_errors_classify_retries_and_recovery() {
    let auth = Error::Provider {
        status: Some(401),
        code: Some("invalid_api_key".to_owned()),
    };
    assert!(auth.retry().is_none());
    assert_eq!(
        auth.failure_kind(),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    let context = Error::Provider {
        status: None,
        code: Some("context_length_exceeded".to_owned()),
    };
    assert!(context.retry().is_none());
    assert_eq!(
        context.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
    for code in [
        "invalid_request_error",
        "invalid_request",
        "model_not_found",
    ] {
        let request = Error::Provider {
            status: None,
            code: Some(code.to_owned()),
        };
        assert!(request.retry().is_none());
        assert_eq!(
            request.failure_kind(),
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        );
    }
    let service = Error::Provider {
        status: Some(500),
        code: Some("server_error".to_owned()),
    };
    assert_eq!(
        service.retry().expect("retry service failure").class,
        RetryClass::Overload
    );
}

/// WebSocket mode must negotiate `/responses`, send the public
/// `response.create` envelope without SSE-only fields, and consume the ordinary
/// Responses event stream without falling back to HTTP/SSE.
#[test]
#[allow(clippy::result_large_err)]
fn websocket_attempt_uses_response_create_protocol() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let server = std::thread::spawn(move || {
        let socket = accept_websocket_peer(&listener);
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set peer read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("set peer write timeout");
        let request_path = Arc::new(Mutex::new(String::new()));
        let captured_path = Arc::clone(&request_path);
        let mut socket =
            tungstenite::accept_hdr(socket, move |request: &WebSocketRequest, response| {
                *captured_path.lock().expect("path lock") = request.uri().path().to_owned();
                Ok(response)
            })
            .expect("upgrade WebSocket");
        let message = socket.read().expect("read response.create");
        let Message::Text(text) = message else {
            panic!("response.create must be a text frame");
        };
        let envelope: Value = serde_json::from_str(text.as_ref()).expect("request JSON");
        assert_eq!(envelope["type"], "response.create");
        assert_eq!(envelope["model"], "test-model");
        assert!(envelope.get("stream").is_none());
        assert!(envelope.get("previous_response_id").is_none());
        assert_eq!(
            request_path.lock().expect("path lock").as_str(),
            "/responses"
        );
        socket
            .send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_ws","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#
                    .into(),
            ))
            .expect("send completed response");
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: "test-key".to_owned(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    join_websocket_peer(server);
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("WebSocket Responses attempt must complete");
    };
    assert_eq!(success.provider_response_id.as_deref(), Some("resp_ws"));
}

/// A rejected WebSocket upgrade must remain on the selected transport and
/// surface the provider's HTTP status without attempting an SSE request.
#[test]
fn websocket_rejected_upgrade_is_terminal() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind upgrade server");
    let address = listener.local_addr().expect("upgrade server address");
    let server = std::thread::spawn(move || {
        let mut socket = accept_websocket_peer(&listener);
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set peer read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("set peer write timeout");
        let mut request = [0_u8; 4096];
        let read = socket.read(&mut request).expect("read upgrade request");
        assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /responses "));
        let body = r#"{"error":{"code":"invalid_api_key"}}"#;
        write!(
            socket,
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write upgrade rejection");
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: "bad-key".to_owned(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    join_websocket_peer(server);
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("authentication rejection must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert_eq!(failure.message, "provider returned HTTP 401");
}

/// Binary and oversized text frames must fail the finite WebSocket attempt
/// before untrusted payloads reach the Responses event parser.
#[test]
fn websocket_rejects_invalid_and_oversized_frames() {
    for message in [
        Message::Binary(vec![0_u8; 8].into()),
        Message::Text("x".repeat(MAX_EVENT_BYTES + 1).into()),
    ] {
        let outcome = run_websocket_message(message, &mut || false);
        assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
    }
    let outcome = run_websocket_message(
        Message::Text(r#"{"type":"error","status":401,"error":{"code":"invalid_api_key"}}"#.into()),
        &mut || false,
    );
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("known WebSocket auth error must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert!(failure.message.contains("invalid_api_key"));
    let outcome = run_websocket_message(
        Message::Text(
            r#"{"type":"response.incomplete","response":{"error":{"code":"context_length_exceeded"}}}"#
                .into(),
        ),
        &mut || false,
    );
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("status-less context error must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
}

/// Individually valid WebSocket events must still fail once their aggregate
/// transport bytes cross the complete-response budget.
#[test]
fn websocket_rejects_cumulative_response_overflow() {
    let event = format!("{{}}{}", " ".repeat(MAX_EVENT_BYTES - 2));
    let messages = (0..65)
        .map(|_| Message::Text(event.clone().into()))
        .collect();
    let outcome = run_websocket_messages(messages, &mut || false);
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
}

/// Cancellation after `response.create` must drop the socket promptly rather
/// than waiting for a peer that never sends a model event or acknowledges
/// close.
#[test]
fn websocket_stalled_peer_cancels_without_close_wait() {
    let mut polls = 0_u8;
    let started = Instant::now();
    let outcome = run_websocket_message(Message::Ping(Vec::new().into()), &mut || {
        polls = polls.saturating_add(1);
        5 <= polls
    });
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert!(started.elapsed() < Duration::from_secs(3));
}

fn run_websocket_message(
    message: Message,
    is_canceled: &mut impl FnMut() -> bool,
) -> AttemptOutcome {
    run_websocket_messages(vec![message], is_canceled)
}

fn run_websocket_messages(
    messages: Vec<Message>,
    is_canceled: &mut impl FnMut() -> bool,
) -> AttemptOutcome {
    let stall = matches!(messages.as_slice(), [Message::Ping(_)]);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let server = std::thread::spawn(move || {
        let socket = accept_websocket_peer(&listener);
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set peer read timeout");
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .expect("set peer write timeout");
        let mut socket = tungstenite::accept(socket).expect("upgrade WebSocket");
        let _ = socket.read().expect("read response.create");
        for message in messages {
            if socket.send(message).is_err() {
                break;
            }
        }
        if stall {
            assert!(matches!(socket.read(), Ok(Message::Pong(_))));
            let _ = socket.read();
        }
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        is_canceled,
        &test_network(),
    );
    join_websocket_peer(server);
    outcome
}

fn accept_websocket_peer(listener: &TcpListener) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("set bounded accept mode");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket
                    .set_nonblocking(false)
                    .expect("restore blocking peer mode");
                return socket;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "WebSocket client did not connect"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept WebSocket request: {error}"),
        }
    }
}

fn join_websocket_peer(server: std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(server.join());
    });
    rx.recv_timeout(Duration::from_secs(7))
        .expect("WebSocket peer must finish within its socket deadlines")
        .expect("WebSocket peer must not panic");
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
                        ErrorKind::ConnectionReset
                            | ErrorKind::BrokenPipe
                            | ErrorKind::UnexpectedEof
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
                transport: Transport::Sse,
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
            transport: Transport::Sse,
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

/// Error bodies must stop at the documented 64 KiB decoded cap before
/// diagnostics or retry classification can retain an arbitrarily large
/// compressed provider response.
#[test]
fn compressed_error_body_read_is_capped_after_decoding() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind error server");
    let address = listener.local_addr().expect("error server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept error request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).expect("read error request");
        let decoded = vec![b'x'; MAX_HTTP_ERROR_BODY_BYTES as usize + 1];
        let body = zstd::stream::encode_all(decoded.as_slice(), 1).expect("encode error body");
        assert!(body.len() < MAX_HTTP_ERROR_BODY_BYTES as usize);
        write!(
            socket,
            "HTTP/1.1 400 Bad Request\r\ncontent-encoding: zstd\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
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

/// A compressed SSE line must be rejected by the existing decoded line bound,
/// and transport statistics must retain their decoded-chunk accounting.
#[test]
fn compressed_sse_line_uses_decoded_limits_and_statistics() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept SSE request");
        let _ = read_http_request(&mut socket);
        let decoded = vec![b'x'; MAX_EVENT_BYTES + 1];
        let body = zstd::stream::encode_all(decoded.as_slice(), 1).expect("encode SSE body");
        assert!(body.len() < MAX_EVENT_BYTES);
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-encoding: zstd\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .expect("write response headers");
        socket.write_all(&body).expect("write compressed SSE body");
    });
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    server.join().expect("join SSE server");
    let AttemptOutcome::Retryable { progress, .. } = outcome else {
        panic!("oversized decoded SSE line must fail retryably");
    };
    assert!(MAX_EVENT_BYTES < progress.response_bytes_received as usize);
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

fn cache_prefix_prompt() -> tau_proto::AgentPromptCreated {
    let mut prompt = minimal_prompt();
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
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(tau_proto::MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text {
                        text: "first stable turn".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            },
        ));
    prompt
}

/// Ensures local correlation changes, appended history, and call suppression
/// retain the existing public Responses lowering needed for prefix reuse.
#[test]
fn responses_request_keeps_stable_lowering_for_local_changes() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
    };
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };
    let prompt = cache_prefix_prompt();

    let stable = build_request(&prompt, &config, &model).expect("stable request");
    let stable_bytes = serde_json::to_vec(&stable).expect("serialize stable request");

    let mut irrelevant = prompt.clone();
    irrelevant.agent_prompt_id = "responses-next".parse().expect("prompt id");
    irrelevant.session_id = "session-next".parse().expect("session id");
    irrelevant.share_user_cache_key = true;
    assert_eq!(
        serde_json::to_vec(
            &build_request(&irrelevant, &config, &model).expect("irrelevant request")
        )
        .expect("serialize"),
        stable_bytes,
        "local correlation fields must not perturb provider bytes"
    );

    let mut next_turn = prompt.clone();
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
    let next = build_request(&next_turn, &config, &model).expect("next request");
    let stable_json = serde_json::to_value(&stable).expect("serialize stable structure");
    let next_json = serde_json::to_value(&next).expect("serialize next structure");
    let stable_input = stable_json["input"].as_array().expect("stable input");
    let next_input = next_json["input"].as_array().expect("next input");
    assert_eq!(&next_input[..stable_input.len()], stable_input);
    assert_eq!(next.instructions, stable.instructions);
    assert_eq!(next.tools, stable.tools);

    let mut disabled = prompt.clone();
    disabled.tool_choice = tau_proto::ToolChoice::None;
    let disabled = build_request(&disabled, &config, &model).expect("disabled request");
    assert_eq!(disabled.tools, stable.tools);
    assert_eq!(disabled.tool_choice.as_deref(), Some("none"));
}

/// Ensures provider-visible model, authority, schema, and effort changes do
/// not accidentally reuse public Responses request lowering for another
/// identity.
#[test]
fn responses_request_exposes_provider_visible_identity_changes() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
    };
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };
    let prompt = cache_prefix_prompt();
    let stable_bytes =
        serde_json::to_vec(&build_request(&prompt, &config, &model).expect("request"))
            .expect("serialize");

    let mut changed_system = prompt.clone();
    changed_system.system_prompt.push('!');
    assert_ne!(
        serde_json::to_vec(
            &build_request(&changed_system, &config, &model).expect("changed system request")
        )
        .expect("serialize"),
        stable_bytes
    );

    let mut changed_tool = prompt.clone();
    changed_tool.tools[0].parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {"key": {"type": "integer"}},
    }));
    assert_ne!(
        serde_json::to_vec(
            &build_request(&changed_tool, &config, &model).expect("changed tool request")
        )
        .expect("serialize"),
        stable_bytes
    );

    let mut changed_effort = prompt.clone();
    changed_effort.model_params.effort = tau_proto::Effort::High;
    assert_ne!(
        serde_json::to_vec(
            &build_request(&changed_effort, &config, &model).expect("changed effort request")
        )
        .expect("serialize"),
        stable_bytes
    );

    let changed_model = AttemptModel {
        id: ModelName::new("other-model"),
    };
    assert_ne!(
        serde_json::to_vec(
            &build_request(&prompt, &config, &changed_model).expect("changed model request")
        )
        .expect("serialize"),
        stable_bytes
    );
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
