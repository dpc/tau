use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::runtime as path_tokio_runtime;

use super::*;

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
        let decoded = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
        let body = zstd::stream::encode_all(decoded.as_slice(), 1).expect("encode SSE body");
        assert!(body.len() < MAX_SSE_LINE_BYTES);
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
    assert!(MAX_SSE_LINE_BYTES < progress.response_bytes_received as usize);
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
