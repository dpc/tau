use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use tokio::runtime as path_tokio_runtime;
use tungstenite::Message;
use tungstenite::handshake::server::Request as WebSocketRequest;
use tungstenite::protocol::frame::Frame;
use tungstenite::protocol::frame::coding::{Data as WebSocketData, OpCode};

use super::*;

/// In-memory trace writer for production transport assertions.
#[derive(Clone, Default)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TraceWriter {
    /// Append formatted trace bytes.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// The in-memory sink has no external buffer.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A real public SSE success must emit exactly one fully qualified backend
/// observation without changing the existing attempt result.
#[test]
fn public_sse_success_emits_private_stage_trace() {
    let output = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let output = output.clone();
            move || output.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let captures = successful_public_sse_captures(false, tau_proto::PromptOperation::Inference);
        assert!(captures.is_empty());
    });
    let trace =
        String::from_utf8(output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert_eq!(
        trace.matches("provider backend stage observation").count(),
        1
    );
    assert!(trace.contains("outcome=\"completed\""), "{trace}");
    assert!(trace.contains("first_input_seen=true"), "{trace}");
    assert!(trace.contains("decode_count=1"), "{trace}");
}

/// Text-only Responses tool-result lowering retains textual output, marks
/// omitted typed images, and never embeds their canonical bytes.
#[test]
fn responses_tool_result_marks_omitted_images_without_byte_egress() {
    let image_bytes = b"responses-image-egress-sentinel";
    let result = tau_proto::ToolResultItem {
        presentation: Default::default(),
        call_id: tau_proto::ToolCallId::new("image-call"),
        tool_type: ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            "text remains".to_owned(),
        )),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: image_bytes.to_vec().into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
    };
    let lowered = lower_item(&ContextItem::ToolResult(result))
        .expect("lower tool result")
        .expect("function tool result is emitted");
    let ResponsesInputItem::Json(lowered) = lowered else {
        panic!("tool result must use JSON input");
    };
    assert_eq!(
        lowered,
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "image-call",
            "output": format!("text remains\n{IMAGE_OMISSION_MARKER}"),
        })
    );
}

/// Responses replay must preserve the exact machine-readable wait interruption
/// framing in the provider's function-call output.
#[test]
fn responses_tool_result_preserves_typed_wait_interruption_fixture() {
    let interruption = "tau_internal: true\nwait_outcome: interrupted\nwait_reason: activating_input\nwait_mode: exact\n\nNew input is queued; retry the wait to consume its target result.";
    let result = tau_proto::ToolResultItem {
        presentation: Default::default(),
        call_id: tau_proto::ToolCallId::new("wait-call"),
        tool_type: ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            interruption.to_owned(),
        )),
        provider_content: Vec::new(),
    };

    let lowered = lower_item(&ContextItem::ToolResult(result))
        .expect("lower tool result")
        .expect("function tool result is emitted");
    let ResponsesInputItem::Json(lowered) = lowered else {
        panic!("tool result must use JSON input");
    };
    assert_eq!(
        lowered,
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "wait-call",
            "output": interruption,
        })
    );
}

/// Responses replay must preserve the plural wait interruption mode byte for
/// byte in its function-call output.
#[test]
fn responses_tool_result_preserves_exact_all_wait_interruption_fixture() {
    let interruption = "tau_internal: true\nwait_outcome: interrupted\nwait_reason: activating_input\nwait_mode: exact_all\n\nNew input is queued; retry the wait to consume its target result.";
    let result = tau_proto::ToolResultItem {
        presentation: Default::default(),
        call_id: tau_proto::ToolCallId::new("wait-all-call"),
        tool_type: ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            interruption.to_owned(),
        )),
        provider_content: Vec::new(),
    };

    let lowered = lower_item(&ContextItem::ToolResult(result))
        .expect("lower plural wait result")
        .expect("function tool result is emitted");
    let ResponsesInputItem::Json(lowered) = lowered else {
        panic!("tool result must use JSON input");
    };
    assert_eq!(
        lowered,
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "wait-all-call",
            "output": interruption,
        })
    );
}

/// Real attempt entry points emit typed build and outbound failures, while
/// cancellation and disabled capture policy emit no diagnostic record.
#[test]
fn debug_capture_policy_and_pre_dispatch_failures_use_real_attempt_path() {
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let sink: Arc<dyn Fn(tau_provider::debug_capture_writer::ProviderDebugCapture) + Send + Sync> =
        Arc::new(move |capture| captured.lock().expect("capture lock").push(capture));
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };
    let config = AttemptConfig {
        base_url: "http://example.invalid".to_owned(),
        api_key: " exact-secret ".to_owned(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    };

    let mut invalid = minimal_prompt();
    invalid.tools.push(tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("custom"),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: ToolType::Custom,
        format: None,
    });
    let outcome = run_attempt_with_capture(
        &invalid,
        &config,
        &model,
        DebugCapture::with_test_sink(true, Arc::clone(&sink)),
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    assert!(matches!(outcome, AttemptOutcome::Terminal(_)));

    let mut environment = BTreeMap::new();
    environment.insert("http_proxy".to_owned(), "not a proxy URL".to_owned());
    let outcome = run_attempt_with_capture(
        &minimal_prompt(),
        &config,
        &model,
        DebugCapture::with_test_sink(true, Arc::clone(&sink)),
        &mut |_| {},
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(environment, None),
    );
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));

    let outcome = run_attempt_with_capture(
        &minimal_prompt(),
        &config,
        &model,
        DebugCapture::with_test_sink(true, Arc::clone(&sink)),
        &mut |_| {},
        &mut || true,
        &test_network(),
    );
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));

    let _ = run_attempt_with_capture(
        &invalid,
        &config,
        &model,
        DebugCapture::with_test_sink(false, Arc::clone(&sink)),
        &mut |_| {},
        &mut || false,
        &test_network(),
    );

    let captures = captures.lock().expect("capture lock");
    assert_eq!(
        captures.len(),
        2,
        "disabled and canceled attempts add nothing"
    );
    assert!(captures.iter().all(|capture| capture.class()
        == tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse));
    let values = captures
        .iter()
        .map(|capture| serde_json::from_slice::<Value>(capture.json()).expect("failure capture"))
        .collect::<Vec<_>>();
    assert_eq!(
        values
            .iter()
            .filter(|value| value["error"]["kind"] == "unsupported_tool")
            .count(),
        1
    );
    let outbound = values
        .iter()
        .find(|value| value["error"]["kind"] == "outbound")
        .expect("outbound capture");
    assert_eq!(outbound["error"]["kind"], "outbound");
    assert_eq!(outbound["error"]["route"], "Proxy");
    assert_eq!(outbound["error"]["phase"], "Configure");
    assert_eq!(outbound["error"]["category"], "InvalidConfiguration");
}

/// The public attempt entry point applies the same enabled or disabled capture
/// policy to ordinary inference and standalone local compaction.
#[test]
fn public_attempt_applies_debug_capture_policy_to_successful_streams() {
    let enabled = successful_public_sse_captures(true, tau_proto::PromptOperation::Inference);
    assert_eq!(enabled.len(), 2);
    assert_eq!(
        enabled[0].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseRequest
    );
    assert_eq!(
        enabled[1].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
    );
    assert!(
        successful_public_sse_captures(false, tau_proto::PromptOperation::Inference).is_empty()
    );
    assert_eq!(
        successful_public_sse_captures(true, tau_proto::PromptOperation::StandaloneCompaction)
            .len(),
        2
    );
}

fn successful_public_sse_captures(
    debug_provider_requests: bool,
    operation: tau_proto::PromptOperation,
) -> Vec<tau_provider::debug_capture_writer::ProviderDebugCapture> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind capture policy server");
    let address = listener
        .local_addr()
        .expect("capture policy server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept capture policy request");
        let _ = read_http_request(&mut socket);
        let body = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"policy-response\",",
            "\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n",
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write capture policy response");
    });
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let sink = Arc::new(move |capture| captured.lock().expect("capture lock").push(capture));
    let mut prompt = minimal_prompt();
    prompt.operation = operation;
    let outcome = DebugCapture::with_test_sink_scope(sink, || {
        run_attempt_with_debug(
            &prompt,
            &AttemptConfig {
                base_url: format!("http://{address}"),
                api_key: String::new(),
                max_output_tokens: 0,
                transport: Transport::Sse,
                prompt_cache: None,
            },
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
            debug_provider_requests,
            &mut |_| {},
            &mut || false,
            &test_network(),
        )
    });
    server.join().expect("join capture policy server");
    assert!(matches!(outcome, AttemptOutcome::Completed(_)));
    std::mem::take(&mut *captures.lock().expect("capture lock"))
}

/// Public Responses usage preserves OpenAI cache reads and writes as separate
/// ordinary-request observations with unknown expiry confidence. This prevents
/// counters from promoting observed reuse into a hard TTL or renewal guarantee.
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
    assert_eq!(
        cache.refresh_reason,
        Some(tau_proto::ProviderCacheRefreshReason::OrdinaryRequest)
    );
    assert_eq!(
        cache.expiry_confidence,
        Some(tau_proto::ProviderCacheExpiryConfidence::Unknown)
    );
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
            ..
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
        reasoning.raw_json(),
        r#"{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"think "},{"type":"reasoning_text","text":"carefully"}],"provider_number":1.2300,"provider_future":true}"#
    );
    let raw_reasoning: serde_json::Value =
        serde_json::from_str(reasoning.raw_json()).expect("reasoning replay JSON");
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

/// Ensures per-event scalar observations and one due borrowed projection do not
/// clone a growing public Responses display prefix.
#[test]
fn borrowed_progress_avoids_display_materialization_when_sample_due() {
    PROGRESS_MATERIALIZATIONS.with(|count| count.set(0));
    let mut state = State::default();
    for index in 0..1_000 {
        state
            .apply_event(&format!(
                r#"{{"type":"response.output_text.delta","output_index":0,"delta":"{index},"}}"#
            ))
            .expect("accepted text delta");
        let progress = state.progress_view();
        assert!(progress.has_timed_semantic_output());
        assert_eq!(progress.response_bytes_received(), 0);
    }
    assert_eq!(
        PROGRESS_MATERIALIZATIONS.with(std::cell::Cell::get),
        0,
        "suppressed samples must not clone the growing display prefix"
    );
    let mut output = Vec::new();
    state.progress_view().visit_display_output(|item| {
        output.push((item.output_index, item.kind, item.text.len()));
    });
    assert_eq!(output.len(), 1);
    assert_eq!(
        PROGRESS_MATERIALIZATIONS.with(std::cell::Cell::get),
        0,
        "a due sample borrows the display accumulator"
    );
}

/// Cumulative non-append replacements renew the display generation, later
/// appends retain it, and a missing lower output index hides the projection.
#[test]
fn borrowed_display_projection_tracks_replacements_appends_and_gaps() {
    PROGRESS_MATERIALIZATIONS.with(|count| count.set(0));
    let mut state = State::default();
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":1,"delta":"hidden"}"#)
        .expect("gapped text");
    let mut displays = Vec::new();
    state.progress_view().visit_display_output(|output| {
        displays.push((
            output.output_index,
            output.generation,
            output.text.to_owned(),
        ));
    });
    assert!(displays.is_empty());

    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"old"}"#)
        .expect("initial contiguous text");
    let replacement = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "new"}],
        },
    });
    state
        .apply_event(&replacement.to_string())
        .expect("valid cumulative replacement");
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"雪"}"#)
        .expect("Unicode append after replacement");

    displays.clear();
    state.progress_view().visit_display_output(|output| {
        displays.push((
            output.output_index,
            output.generation,
            output.text.to_owned(),
        ));
    });
    assert_eq!(
        displays,
        vec![
            (0, DisplayGeneration(1), "new雪".to_owned()),
            (1, DisplayGeneration::default(), "hidden".to_owned()),
        ]
    );
    assert_eq!(PROGRESS_MATERIALIZATIONS.with(std::cell::Cell::get), 0);
}

/// Authoritative terminal output inherits the streamed slot generation and
/// renews it before the extension can cursor-slice a non-prefix replacement.
#[test]
fn terminal_replacement_renews_borrowed_display_generation() {
    let mut state = State::default();
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"old!"}"#)
        .expect("streamed text");
    let terminal = serde_json::json!({
        "type": "response.completed",
        "response": {
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "雪 replacement"}],
            }],
        },
    });
    state
        .apply_event(&terminal.to_string())
        .expect("authoritative replacement");
    let mut display = None;
    state.progress_view().visit_display_output(|output| {
        display = Some((output.generation, output.text.to_owned()));
    });
    assert_eq!(
        display,
        Some((DisplayGeneration(1), "雪 replacement".to_owned()))
    );
    assert_eq!(
        state.progress_items()[0].display_generation,
        DisplayGeneration(1)
    );
}

/// Removing an emitted display channel invalidates its generation so later
/// unrelated multibyte text cannot reuse the old byte cursor.
#[test]
fn display_disappearance_invalidates_later_reappearance() {
    let mut state = State::default();
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"old!"}"#)
        .expect("streamed text");
    for text in ["", "雪 replacement"] {
        let replacement = serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            },
        });
        state
            .apply_event(&replacement.to_string())
            .expect("accepted cumulative replacement");
    }
    let mut display = None;
    state.progress_view().visit_display_output(|output| {
        display = Some((output.generation, output.text.to_owned()));
    });
    assert_eq!(
        display,
        Some((DisplayGeneration(1), "雪 replacement".to_owned()))
    );
}

/// A terminal response output array is authoritative when a provider omits
/// incremental item events, including for plain reasoning followed by text.
#[test]
fn terminal_output_fallback_materializes_plain_reasoning_and_text() {
    let mut state = State::default();
    state.apply_event(r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"fallback thought"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback answer"}]}]}}"#,
    )
    .expect("terminal output");

    assert_eq!(state.terminal, Some(TerminalKind::Completed));
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

/// The shared attempt guard treats assistant text, reasoning text, and function
/// arguments as independent channels and rejects each candidate atomically.
#[test]
fn repetition_guard_rejects_independent_delta_channels_before_mutation() {
    let repeated = "x".repeat(1_024);

    let mut assistant = State::default();
    assistant
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"accepted"}"#)
        .expect("initial assistant text");
    let event = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": repeated,
    });
    assert!(matches!(
        assistant.apply_event(&event.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    assert!(matches!(
        assistant.progress().output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "accepted".to_owned(),
        }]
    ));

    let mut reasoning = State::default();
    reasoning
        .apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_guard","summary":[],"content":[]}}"#)
        .expect("reasoning item");
    let event = serde_json::json!({
        "type": "response.reasoning_text.delta",
        "item_id": "rs_guard",
        "output_index": 0,
        "content_index": 0,
        "delta": repeated,
    });
    assert!(matches!(
        reasoning.apply_event(&event.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    assert!(
        reasoning.progress().output_items.is_empty(),
        "rejected reasoning must not become transient output"
    );

    let mut arguments = State::default();
    arguments
        .apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_guard","name":"run","arguments":""}}"#)
        .expect("function item");
    let event = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": repeated,
    });
    assert!(matches!(
        arguments.apply_event(&event.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    let ContextItem::ToolCall(call) = &arguments.items[0].item else {
        panic!("function call remains");
    };
    assert_eq!(call.raw_arguments_json.as_deref(), Some(""));
}

/// Cumulative item snapshots and authoritative terminal output use replacement
/// validation rather than bypassing the attempt guard.
#[test]
fn repetition_guard_rejects_cumulative_and_terminal_replacements() {
    let repeated = "x".repeat(1_024);

    let mut cumulative = State::default();
    cumulative
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"accepted"}"#)
        .expect("initial assistant text");
    let event = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": repeated}],
        },
    });
    assert!(matches!(
        cumulative.apply_event(&event.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    assert!(matches!(
        cumulative.progress().output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "accepted".to_owned(),
        }]
    ));

    for item in [
        serde_json::json!({
            "type": "reasoning",
            "id": "rs_terminal",
            "summary": [],
            "content": [{"type": "reasoning_text", "text": repeated}],
        }),
        serde_json::json!({
            "type": "function_call",
            "call_id": "call_terminal",
            "name": "run",
            "arguments": format!("{{}}{}", " ".repeat(1_024)),
        }),
    ] {
        let mut terminal = State::default();
        let event = serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_guard", "output": [item]},
        });
        assert!(matches!(
            terminal.apply_event(&event.to_string()),
            Err(Error::RepetitionDetected(_))
        ));
        assert!(terminal.items.is_empty());
        assert_eq!(terminal.terminal, None);
        assert!(terminal.response_id.is_none());
    }
}

/// Final text snapshots must guard whole provider content without rejecting an
/// ordinary completion or leaving repeated text in the transient slot.
#[test]
fn output_text_done_snapshots_are_guarded_and_atomic() {
    let mut accepted = State::default();
    accepted
        .apply_event(
            r#"{"type":"response.output_text.done","output_index":0,"text":"complete answer"}"#,
        )
        .expect("ordinary final snapshot");
    assert!(matches!(
        accepted.progress().output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "complete answer".to_owned(),
        }]
    ));

    let repeated = "x".repeat(1_024);
    let mut rejected = State::default();
    let event = serde_json::json!({
        "type": "response.output_text.done",
        "output_index": 0,
        "text": repeated,
    });
    assert!(matches!(
        rejected.apply_event(&event.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    assert!(
        rejected.items.is_empty(),
        "rejected final snapshot must not become transient output"
    );
}

/// Repeated ordinary prose must remain accepted when it stays below the
/// conservative thresholds or its suffix changes instead of forming a loop.
#[test]
fn repetition_guard_accepts_short_changing_and_below_threshold_text() {
    let mut short_words = State::default();
    short_words
        .apply_event(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "again ".repeat(7),
            })
            .to_string(),
        )
        .expect("short repeated words are not a loop");

    let mut changing_prefixes = State::default();
    for ordinal in 0..160 {
        changing_prefixes
            .apply_event(
                &serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": format!("prefix {ordinal:04} "),
                })
                .to_string(),
            )
            .expect("changing suffix is not a loop");
    }

    let line = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz\n";
    let mut below_threshold_lines = State::default();
    below_threshold_lines
        .apply_event(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": line.repeat(7),
            })
            .to_string(),
        )
        .expect("seven repeated long lines are below the line-loop threshold");
}

/// Complete reasoning and Function-argument snapshots must pass through the
/// guard before they can replace the accepted slot state.
#[test]
fn repetition_guard_rejects_reasoning_and_function_done_snapshots_atomically() {
    let repeated = "x".repeat(1_024);
    let mut reasoning = State::default();
    reasoning
        .apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_done","summary":[],"content":[]}}"#)
        .expect("reasoning item");
    let reasoning_done = serde_json::json!({
        "type": "response.reasoning_text.done",
        "item_id": "rs_done",
        "output_index": 0,
        "content_index": 0,
        "text": repeated,
    });
    assert!(matches!(
        reasoning.apply_event(&reasoning_done.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    assert!(reasoning.items[0].reasoning_parts.is_empty());
    assert!(reasoning.items[0].reasoning_text.is_none());

    let mut function = State::default();
    function
        .apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_done","name":"run","arguments":""}}"#)
        .expect("function item");
    let function_done = serde_json::json!({
        "type": "response.function_call_arguments.done",
        "output_index": 0,
        "arguments": format!("{{}}{}", " ".repeat(1_024)),
    });
    assert!(matches!(
        function.apply_event(&function_done.to_string()),
        Err(Error::RepetitionDetected(_))
    ));
    let ContextItem::ToolCall(call) = &function.items[0].item else {
        panic!("function call remains");
    };
    assert_eq!(call.raw_arguments_json.as_deref(), Some(""));
}

/// Empty assistant deltas must not claim an output index and allow later output
/// to bypass the response assembler's contiguous-prefix requirement.
#[test]
fn empty_assistant_delta_does_not_close_a_missing_output_index_gap() {
    let mut state = State::default();
    assert!(
        !state
            .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":""}"#)
            .expect("empty delta is accepted but non-semantic")
    );
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":1,"delta":"later"}"#)
        .expect("later output remains pending");
    assert!(state.progress().output_items.is_empty());
    assert!(matches!(
        state.apply_event(r#"{"type":"response.completed","response":{}}"#),
        Err(Error::UnsupportedOutput)
    ));
}

/// An SSE repetition after visible progress must close the finite attempt
/// without retrying while retaining the progress bit that requests one clear.
#[test]
fn sse_repetition_is_nonretryable_and_requests_clear_after_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        let repeated = "x".repeat(1_024);
        let body = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"accepted\"}}\n\n\
             data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{repeated}\"}}\n\n"
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
            prompt_cache: None,
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
        panic!("repetition must close rather than retry");
    };
    assert_eq!(failure.stop_reason, ProviderStopReason::RepetitionDetected);
    assert_eq!(failure.failure_kind, None);
    assert!(failure.progress.has_timed_semantic_output);
    assert!(matches!(
        failure.progress.output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "accepted".to_owned(),
        }]
    ));
}

/// WebSocket terminal full-content repetition before visible progress must
/// close without retry or a spurious clear request.
#[test]
fn websocket_terminal_repetition_is_nonretryable_without_clear() {
    let repeated = "x".repeat(1_024);
    let event = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_guard",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": repeated}],
            }],
        },
    });
    let outcome = run_websocket_message(Message::Text(event.to_string().into()), &mut || false);
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("repetition must close rather than retry");
    };
    assert_eq!(failure.stop_reason, ProviderStopReason::RepetitionDetected);
    assert_eq!(failure.failure_kind, None);
    assert!(!failure.progress.has_timed_semantic_output);
    assert!(failure.progress.output_items.is_empty());
}

/// Terminal replacement retains exact raw provider item syntax for transcript
/// replay rather than reserializing its key order, numeric spelling, or extras.
#[test]
fn terminal_output_retains_exact_raw_message_sidecar() {
    let raw_message = r#"{"type":"message","z":1.2300,"id":"msg_terminal","status":"completed","future":true,"role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"terminal","annotations":[]}]}"#;
    let mut state = State::default();
    state
        .apply_event(&format!(
            r#"{{"type":"response.completed","response":{{"output":[{raw_message}]}}}}"#
        ))
        .expect("terminal output");
    let output = state.output_items();
    let ContextItem::Message(message) = &output[0] else {
        panic!("terminal message");
    };
    assert_eq!(message.responses_raw_json.as_deref(), Some(raw_message));
    let mut prompt = minimal_prompt();
    prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::AssistantResponse(
            tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: output,
                usage: None,
            },
        ));
    let request = build_request(
        &prompt,
        &AttemptConfig {
            base_url: "https://example.test/v1".to_owned(),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    let request = serde_json::to_string(&request).expect("serialize request");
    assert!(
        request.contains(raw_message),
        "terminal sidecar must survive transcript replay unchanged: {request}"
    );
}

/// Both SSE and WebSocket feed one decoded projection into the shared
/// assembler; when either transport delivers index one before index zero,
/// progress waits and then projects the completed prefix in provider
/// output-index order.
#[test]
fn shared_assembler_orders_out_of_order_progress_and_terminal_output() {
    let mut state = State::default();
    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":1,"delta":"second"}"#)
        .expect("index one delta");
    assert!(
        state.progress().output_items.is_empty(),
        "index one must remain hidden while index zero is unresolved"
    );

    state
        .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"first"}"#)
        .expect("index zero delta");
    let progress = state.progress();
    assert_eq!(
        progress
            .output_items
            .iter()
            .map(|item| item.output_index)
            .collect::<Vec<_>>(),
        [0, 1]
    );

    state
        .apply_event(r#"{"type":"response.completed","response":{"id":"resp_ordered"}}"#)
        .expect("contiguous terminal response");
    let output = state.output_items();
    assert!(matches!(
        output.as_slice(),
        [
            ContextItem::Message(MessageItem {
                content: first,
                ..
            }),
            ContextItem::Message(MessageItem {
                content: second,
                ..
            }),
        ] if first == &vec![ContentPart::Text { text: "first".to_owned() }]
            && second == &vec![ContentPart::Text { text: "second".to_owned() }]
    ));
}

/// The SSE production assembler entry decodes and indexes each event exactly
/// once before applying semantic state.
#[test]
fn sse_assembler_entry_decodes_event_once() {
    super::decoded_event::reset_test_counts();
    let mut state = State::default();
    state
        .apply_event(r#"{"type":"response.heartbeat"}"#)
        .expect("heartbeat");
    assert_eq!(super::decoded_event::test_counts(), (1, 1));
}

/// A terminal response may not turn a sparse stream into a silently reordered
/// transcript, and a present terminal output must use the authoritative array
/// shape.
#[test]
fn shared_assembler_rejects_sparse_or_invalid_terminal_output_shape() {
    let mut sparse = State::default();
    sparse
        .apply_event(r#"{"type":"response.output_text.delta","output_index":1,"delta":"orphaned"}"#)
        .expect("out-of-order stream event remains pending");
    assert!(matches!(
        sparse.apply_event(r#"{"type":"response.completed","response":{}}"#),
        Err(Error::UnsupportedOutput)
    ));

    let mut invalid_terminal = State::default();
    assert!(matches!(
        invalid_terminal.apply_event(r#"{"type":"response.completed","response":{"output":{}}}"#),
        Err(Error::UnsupportedOutput)
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

    let replay = ContextItem::Reasoning(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{"type":"reasoning","id":"rs_raw","summary":[],"content":[{"type":"reasoning_text","text":"replay me"}],"provider_number":1.2300,"provider_future":17}"#
        )
        .expect("valid reasoning item"),
    );
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
            prompt_cache: None,
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

/// The SSE `[DONE]` terminal dialect uses the same provider-index projection
/// as event terminals: index one waits for index zero, then both progress and
/// durable output retain index order.
#[test]
fn sse_done_orders_out_of_order_indices_and_rejects_sparse_output() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"second\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"first\"}\n\n",
            "data: [DONE]\n\n",
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let mut updates = Vec::new();
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |progress| updates.push(progress.materialize_output()),
        &mut || false,
        &test_network(),
    );
    server.join().expect("join SSE server");
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("contiguous SSE stream must complete");
    };
    assert_eq!(
        updates[0].len(),
        0,
        "index one must not project before index zero"
    );
    assert_eq!(
        updates[1]
            .iter()
            .map(|item| item.output_index)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(matches!(
        success.output_items.as_slice(),
        [
            ContextItem::Message(MessageItem {
                content: first,
                ..
            }),
            ContextItem::Message(MessageItem {
                content: second,
                ..
            }),
        ] if first == &vec![ContentPart::Text { text: "first".to_owned() }]
            && second == &vec![ContentPart::Text { text: "second".to_owned() }]
    ));
}

/// A sparse SSE stream must fail at its literal `[DONE]` sentinel rather than
/// publishing a later output index as a valid completed response.
#[test]
fn sse_done_rejects_sparse_output_indices() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"orphaned\"}\n\n",
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
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    server.join().expect("join SSE server");
    assert!(matches!(outcome, AttemptOutcome::Terminal(_)));
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

/// The shared assembler caps index-driven slot allocation at its documented
/// per-attempt bound.
#[test]
fn output_index_limit_rejects_first_out_of_range_slot() {
    let mut state = State::default();
    let last = MAX_OUTPUT_ITEMS - 1;
    state
        .apply_event(&format!(
            r#"{{"type":"response.output_text.delta","output_index":{last},"delta":"last"}}"#
        ))
        .expect("last allowed index");
    assert!(matches!(
        state.apply_event(&format!(
            r#"{{"type":"response.output_text.delta","output_index":{MAX_OUTPUT_ITEMS},"delta":"too far"}}"#
        )),
        Err(Error::UnsupportedOutput)
    ));
}

/// Authoritative terminal arrays share the documented item-cardinality limit
/// with streamed indices, accepting the exact limit and rejecting one more.
#[test]
fn terminal_output_item_limit_is_exact() {
    let output_item = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "item"}],
    });
    for (count, expected) in [
        (MAX_OUTPUT_ITEMS as usize, true),
        (MAX_OUTPUT_ITEMS as usize + 1, false),
    ] {
        let mut state = State::default();
        let event = serde_json::json!({
            "type": "response.completed",
            "response": {"output": vec![output_item.clone(); count]},
        });
        assert_eq!(
            state.apply_event(&event.to_string()).is_ok(),
            expected,
            "terminal output count {count}"
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
        assert_eq!(state.terminal, None);
        assert!(state.response_id.is_none());
        assert!(matches!(
            state.output_items().as_slice(),
            [ContextItem::ReasoningText(_), ContextItem::Reasoning(_)]
        ));
    }
}

/// Replay validation rejects provider-invalid reasoning even after canonical
/// opaque-item validation has accepted its raw and structured equivalence.
#[test]
fn invalid_reasoning_replay_authorities_are_rejected() {
    let invalid = [
        tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"reasoning","encrypted_content":"SEALED","content":[{"type":"reasoning_text","text":"thought"}]}"#
        )
        .expect("canonical but provider-invalid reasoning"),
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}]}"#,
        )
        .expect("canonical but provider-invalid reasoning"),
    ];
    for item in invalid {
        assert!(matches!(
            lower_item(&ContextItem::Reasoning(item)),
            Err(Error::UnsupportedOutput)
        ));
    }
}

/// Completed public Responses reasoning requires the original raw item rather
/// than synthesizing replay JSON from the structured event.
#[test]
fn completed_reasoning_without_raw_json_is_rejected_before_mutation() {
    let item = serde_json::json!({
        "type": "reasoning",
        "id": "rs_missing_raw",
        "summary": [],
        "content": []
    });
    let mut slot = Slot::new(0);

    assert!(matches!(
        slot.apply_item(&item, OutputItemPhase::TerminalFallback, None),
        Err(Error::UnsupportedOutput)
    ));
    assert_eq!(slot.state, SlotState::Empty);
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
    state
        .apply_event(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"run","arguments":""}}"#)
        .expect("function call item");
    assert!(
        state
            .apply_event(r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{ \"path\""}"#)
            .expect("argument delta")
    );
    let done = r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{ \"path\" : \"/tmp\" }"}"#;
    assert!(state.apply_event(done).expect("argument completion"));
    assert!(!state.apply_event(done).expect("duplicate completion"));
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
    assert_eq!(state.terminal, Some(TerminalKind::Completed));
    assert_eq!(state.response_id.as_deref(), Some("resp_1"));
}

/// Canonical max-output incompletion must reconcile terminal prose, usage, and
/// response identity while producing the narrow output-limit terminal marker.
#[test]
fn max_output_incomplete_preserves_partial_prose_usage_and_identity() {
    let mut state = State::default();
    state
        .apply_event(
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"partial answer"}"#,
        )
        .expect("partial prose");
    state
        .apply_event(
            r#"{"type":"response.incomplete","id":"adjacent-id","incomplete_details":{"reason":"content_filter"},"response":{"id":"resp_limited","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":11,"output_tokens":7}}}"#,
        )
        .expect("max-output terminal");

    assert_eq!(state.terminal, Some(TerminalKind::MaxOutputTokens));
    assert_eq!(state.response_id.as_deref(), Some("resp_limited"));
    assert_eq!(
        state
            .usage
            .as_ref()
            .expect("terminal usage")
            .prompt_sent_tokens,
        11
    );
    assert_eq!(
        state
            .usage
            .as_ref()
            .expect("terminal usage")
            .response_received_tokens,
        7
    );
    assert!(matches!(
        state.output_items().as_slice(),
        [ContextItem::Message(message)]
            if message.content == vec![ContentPart::Text {
                text: "partial answer".to_owned(),
            }]
    ));
}

/// Unknown incomplete reasons must retain provider-failure behavior and cannot
/// borrow max-output authority from an unrelated nested identifier.
#[test]
fn unknown_incomplete_reason_remains_a_failure() {
    let mut state = State::default();
    assert!(matches!(
        state.apply_event(
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"metadata":{"reason":"max_output_tokens"}}}"#
        ),
        Err(Error::StreamFailure)
    ));
    assert_eq!(state.terminal, None);

    let mut top_level_only = State::default();
    assert!(matches!(
        top_level_only.apply_event(
            r#"{"type":"response.incomplete","incomplete_details":{"reason":"max_output_tokens"},"response":{"incomplete_details":{"reason":"content_filter"}}}"#
        ),
        Err(Error::StreamFailure)
    ));
}

/// Keepalive and empty/unknown events must not report qualifying semantic
/// progress, while a non-empty text delta does renew stream-idle time.
#[test]
fn qualifying_stream_progress_excludes_heartbeats_and_empty_events() {
    let mut state = State::default();

    assert!(
        !state
            .apply_event(r#"{"type":"response.heartbeat"}"#)
            .expect("unknown heartbeat event")
    );
    assert!(
        !state
            .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":""}"#)
            .expect("empty text delta")
    );

    assert!(
        state
            .apply_event(r#"{"type":"response.output_text.delta","output_index":0,"delta":"drip"}"#)
            .expect("text drip")
    );
    assert!(
        !state
            .apply_event(r#"{"type":"response.heartbeat"}"#)
            .expect("later heartbeat event")
    );
}

/// Item completion that changes only provider status, IDs, or sidecars must
/// not renew idle time after the same message or Function semantics arrived.
#[test]
fn item_metadata_completion_does_not_renew_semantic_idle() {
    let mut state = State::default();
    assert!(
        state
            .apply_event(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"drip"}]}}"#,
            )
            .expect("message item")
    );
    assert!(
        !state
            .apply_event(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"message-1","status":"completed","type":"message","role":"assistant","content":[{"type":"output_text","text":"drip"}]}}"#,
            )
            .expect("message metadata completion")
    );

    assert!(
        state
            .apply_event(
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call-1","name":"run","arguments":"{\"x\":1}"}}"#,
            )
            .expect("Function item")
    );
    assert!(
        !state
            .apply_event(
                r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"item-1","status":"completed","type":"function_call","call_id":"call-1","name":"run","arguments":"{\"x\":1}","provider_metadata":"only metadata changed"}}"#,
            )
            .expect("Function metadata completion")
    );
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
        assert_eq!(request["prompt_cache_key"], "tau:agent-test");
        assert_eq!(request["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(request["prompt_cache_options"]["ttl"], "30m");
        assert!(request.get("prompt_cache_retention").is_none());
        assert_eq!(
            request["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
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
        let oversized_raw = serde_json::json!({
            "type": "response.unknown",
            "padding": "x".repeat(debug_capture::MAX_RESPONSE_EVENT_BYTES),
        });
        let terminal = "data: {\"type\":\"response.completed\",\"diagnostic\":\"DaTa:ImAgE/PNG;base64,raw-event-canary\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n";
        let body = format!("data: {oversized_raw}\n\n{terminal}");
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let debug_capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |capture| captured.lock().expect("capture lock").push(capture)),
    );
    let mut prompt = prompt_with_replayed_user_text();
    prompt.system_prompt = "before  capture-secret  after".to_owned();
    let tau_proto::ContextBlock::AssistantResponse(block) = &mut prompt.context.blocks[1] else {
        panic!("assistant replay block");
    };
    let ContextItem::Message(message) = &mut block.output_items[0] else {
        panic!("assistant replay message");
    };
    message.responses_raw_json = Some(
        r#"{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"assistant replay"}],"diagnostic":"DATA:Image/PNG;base64,replay-sidecar-canary"}"#
            .to_owned(),
    );
    let ContextItem::ToolCall(call) = &mut block.output_items[1] else {
        panic!("assistant replay tool call");
    };
    call.responses_envelope
        .as_mut()
        .expect("tool envelope")
        .extra_fields = Some(tau_proto::CborValue::Map(vec![
        (
            tau_proto::CborValue::Text("diagnostic".to_owned()),
            tau_proto::CborValue::Text("Data:IMAGE/png;base64,function-extra-canary".to_owned()),
        ),
        (
            tau_proto::CborValue::Text("prefix capture-secret suffix".to_owned()),
            tau_proto::CborValue::Text("credential-value  capture-secret  canary".to_owned()),
        ),
        (
            tau_proto::CborValue::Text("prefix[REDACTED]suffix".to_owned()),
            tau_proto::CborValue::Text("collision-canary".to_owned()),
        ),
    ]));
    let outcome = run_attempt_with_capture(
        &prompt,
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: " capture-secret ".to_owned(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: Some(PromptCachePolicy::Explicit),
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        debug_capture,
        &mut |_| {},
        &mut || false,
        &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
    );
    server.join().expect("join test server");
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("Responses SSE attempt must complete");
    };
    assert_eq!(success.provider_response_id.as_deref(), Some("resp_1"));
    let captures = captures.lock().expect("capture lock");
    assert_eq!(captures.len(), 2);
    assert_eq!(
        captures[0].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseRequest
    );
    assert_eq!(
        captures[1].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::HttpSseResponse
    );
    let request: Value = serde_json::from_slice(captures[0].json()).expect("request capture");
    assert_eq!(request["body"]["stream"], true);
    let request_text = String::from_utf8_lossy(captures[0].json());
    assert!(!request_text.contains("replay-sidecar-canary"));
    assert!(!request_text.contains("function-extra-canary"));
    assert!(!request_text.contains("capture-secret"));
    assert!(!request_text.contains("collision-canary"));
    assert!(request_text.contains("projected_key_collision"));
    let response: Value = serde_json::from_slice(captures[1].json()).expect("response capture");
    assert_eq!(response["provider_response_id"], "resp_1");
    assert_eq!(
        response["raw_events"].as_array().expect("raw events").len(),
        1
    );
    assert_eq!(
        response["raw_events"][0]["diagnostic"],
        "[image data omitted]"
    );
    assert_eq!(response["raw_events_truncated"], true);
}

/// Ensures the real SSE callback path reads borrowed cadence inputs and display
/// text for a growing stream; only durable terminal output materializes.
#[test]
fn sse_suppressed_progress_samples_do_not_materialize_display_slots() {
    PROGRESS_MATERIALIZATIONS.with(|count| count.set(0));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        let deltas = (0..1_000)
            .map(|index| {
                format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{index},\"}}\n\n"
                )
            })
            .collect::<String>();
        let output = (0..1_000)
            .map(|index| format!("{index},"))
            .collect::<String>();
        let body = format!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}}}\n\n{deltas}data: {{\"type\":\"response.completed\",\"response\":{{\"output\":[{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{output}\"}}]}}]}}}}\n\n"
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let mut due_samples = 0;
    let outcome = run_attempt_with_debug(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        false,
        &mut |update| {
            let AttemptUpdate::Progress(progress) = update else {
                return;
            };
            std::hint::black_box(progress.response_bytes_received());
            std::hint::black_box(progress.has_timed_semantic_output());
            if due_samples == 0 && progress.has_timed_semantic_output() {
                let mut displays = 0;
                progress.visit_display_output(|output| {
                    displays += 1;
                    std::hint::black_box(output.text);
                });
                assert_eq!(displays, 1);
                due_samples += 1;
            }
        },
        &mut || false,
        &test_network(),
    );
    server.join().expect("join SSE server");
    assert!(matches!(outcome, AttemptOutcome::Completed(_)));
    assert_eq!(due_samples, 1);
    assert_eq!(
        PROGRESS_MATERIALIZATIONS.with(std::cell::Cell::get),
        1,
        "only the separate durable terminal may materialize"
    );
}

/// Terminal materialization must move message and reasoning display buffers
/// while preserving their shared provider index and ordering.
#[test]
fn terminal_projection_moves_owned_display_buffers() {
    let message = "m".repeat(4_096);
    let reasoning = "r".repeat(4_096);
    let message_pointer = message.as_ptr();
    let reasoning_pointer = reasoning.as_ptr();
    let mut slot = Slot::new(0);
    slot.item = ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: message }],
        phase: None,
        responses_raw_json: None,
    });
    slot.state = SlotState::Message;
    slot.reasoning_text = Some(ReasoningTextItem {
        kind: ReasoningTextKind::Full,
        text: reasoning,
    });
    let mut state = State::default();
    state.items.push(slot);

    let (items, display) = state.take_output_items();
    assert_eq!(display.len(), 2);
    assert!(display.iter().all(|item| item.output_index == 0));
    let ContextItem::ReasoningText(reasoning) = &items[0] else {
        panic!("reasoning display must precede its durable slot");
    };
    let ContextItem::Message(message) = &items[1] else {
        panic!("message must retain provider order");
    };
    let ContentPart::Text { text } = &message.content[0] else {
        panic!("assistant content must be text");
    };
    assert_eq!(reasoning.text.as_ptr(), reasoning_pointer);
    assert_eq!(text.as_ptr(), message_pointer);
}

/// An empty assistant message remains durable provider shape but must not
/// invent first-semantic-output timing during the terminal flush.
#[test]
fn empty_message_terminal_preserves_content_free_timing() {
    let mut slot = Slot::new(0);
    slot.item = ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: Vec::new(),
        phase: None,
        responses_raw_json: None,
    });
    slot.state = SlotState::Message;
    let mut state = State::default();
    state.items.push(slot);
    let has_timed_semantic_output = state.has_qualifying_stream_progress();
    let (output_items, terminal_display) = state.take_output_items();
    let success = AttemptSuccess {
        output_items,
        stop_reason: ProviderStopReason::EndTurn,
        usage: None,
        response_bytes_received: 0,
        terminal_display,
        has_timed_semantic_output,
        provider_response_id: None,
    };

    assert_eq!(success.output_items.len(), 1);
    assert!(!success.has_timed_semantic_output());
}

/// Public Responses requests must lower every harness-effective effort to the
/// exact spelling the API accepts.
#[test]
fn request_lowers_every_reasoning_effort_to_exact_wire_spelling() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    };
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };

    for (effort, expected) in [
        (tau_proto::Effort::Off, "none"),
        (tau_proto::Effort::Minimal, "minimal"),
        (tau_proto::Effort::Low, "low"),
        (tau_proto::Effort::Medium, "medium"),
        (tau_proto::Effort::High, "high"),
        (tau_proto::Effort::XHigh, "xhigh"),
        (tau_proto::Effort::Max, "max"),
    ] {
        let mut prompt = minimal_prompt();
        prompt.model_params.effort = effort;
        let request = build_request(&prompt, &config, &model).expect("request");
        let request = serde_json::to_value(request).expect("serialize request");

        assert_eq!(request["reasoning"]["effort"], expected);
    }
}

/// Sender route context keeps the original function arguments and compact tool
/// result, but must not lower a routed body as provider assistant output.
#[test]
fn message_route_context_omits_outbound_body_from_assistant_wire_content() {
    const BODY: &str = "CLANK2AE7_RESPONSES_CANARY";
    let mut prompt = minimal_prompt();
    prompt.context.blocks = vec![
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
            items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::User,
                content: vec![ContentPart::Text {
                    text: format!("<tau_internal>received {BODY}&lt;/tau_internal&gt;"),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        }),
    ];
    let request = build_request(
        &prompt,
        &AttemptConfig {
            base_url: "https://example.test/v1".to_owned(),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    let request = serde_json::to_value(request).expect("serialize request");

    assert!(
        request["input"][0]["arguments"]
            .as_str()
            .is_some_and(|arguments| arguments.contains(BODY))
    );
    assert_eq!(request["input"][1]["output"], "Message sent");
    assert_eq!(request["input"][2]["role"], "user");
    assert!(
        request["input"][2]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains(BODY))
    );
    assert!(
        !request["input"]
            .as_array()
            .expect("input array")
            .iter()
            .any(|item| {
                item["role"] == "assistant" && item["content"].to_string().contains(BODY)
            }),
        "the Responses request must not contain a synthetic assistant replay"
    );
}

/// Public Responses must send only the approved legacy automatic-cache fields
/// and leave its `instructions` representation unchanged.
#[test]
fn request_lowers_legacy_prompt_cache_retention() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: Some(PromptCachePolicy::Legacy(PromptCacheRetention::InMemory)),
    };
    let request = build_request(
        &minimal_prompt(),
        &config,
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    let request = serde_json::to_value(request).expect("serialize request");

    assert_eq!(request["prompt_cache_key"], "tau:agent-test");
    assert_eq!(request["prompt_cache_retention"], "in_memory");
    assert!(request.get("prompt_cache_options").is_none());
    assert_eq!(request["instructions"], "test system");
}

/// Explicit caching must retain top-level instruction authority and mark only
/// the first Tau-constructed input-text block.
#[test]
fn request_lowers_explicit_prompt_cache_at_first_input_text() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: Some(PromptCachePolicy::Explicit),
    };
    let request = build_request(
        &cache_prefix_prompt(),
        &config,
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    let request = serde_json::to_value(request).expect("serialize request");

    assert_eq!(request["instructions"], "stable system authority");
    assert_eq!(request["prompt_cache_key"], "tau:agent-test");
    assert_eq!(request["prompt_cache_options"]["mode"], "explicit");
    assert_eq!(request["prompt_cache_options"]["ttl"], "30m");
    assert!(request.get("prompt_cache_retention").is_none());
    assert_eq!(
        request["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
}

/// Explicit caching must fail before egress when replay supplies no eligible
/// Tau-constructed input text to carry the required provider marker.
#[test]
fn explicit_prompt_cache_rejects_missing_input_text() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: Some(PromptCachePolicy::Explicit),
    };
    assert!(matches!(
        build_request(
            &minimal_prompt(),
            &config,
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
        ),
        Err(Error::InvalidRequest)
    ));
}

/// The explicit boundary must select only the earliest constructed input text
/// and leave raw assistant replay and tool transcript items untouched.
#[test]
fn explicit_prompt_cache_marks_only_earliest_constructed_input() {
    let mut prompt = cache_prefix_prompt();
    prompt.context.blocks.push(tau_proto::ContextBlock::UserInput(
        tau_proto::UserInputBlock {
            items: vec![
                ContextItem::Message(tau_proto::MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "replayed assistant".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: Some(
                        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"old"}]}"#.to_owned(),
                    ),
                }),
                ContextItem::ToolResult(tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: tau_proto::ToolCallId::new("call-1"),
                    tool_type: ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse {
                        raw: tau_proto::CborValue::Text("tool result".to_owned()),
                        headers: Vec::new(),
                        body: "tool result".to_owned(),
                    },
                    provider_content: Vec::new(),
                }),
                ContextItem::Message(tau_proto::MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text {
                        text: "second input".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                }),
            ],
        },
    ));
    let request = build_request(
        &prompt,
        &AttemptConfig {
            base_url: "https://example.test/v1".to_owned(),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: Some(PromptCachePolicy::Explicit),
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
    )
    .expect("request");
    let request = serde_json::to_value(request).expect("serialize request");
    let input = request["input"].as_array().expect("input array");
    assert_eq!(
        input[0]["content"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
    assert_eq!(
        serde_json::to_string(&request)
            .expect("request JSON")
            .matches("prompt_cache_breakpoint")
            .count(),
        1
    );
    assert!(
        input[1]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert!(input[2].get("prompt_cache_breakpoint").is_none());
    assert!(
        input[3]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
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

/// A public WebSocket terminal diagnostic must retain ordinary provider detail
/// while visibly escaping every control and terminal-unsafe Unicode scalar.
#[test]
fn websocket_provider_error_message_escapes_unsafe_detail_without_redaction() {
    let outcome = terminal(
        Error::Provider {
            status: Some(400),
            code: Some("token=example-value\n\t\x1b[2J\u{202e}done".to_owned()),
        },
        State::default().progress(),
    );
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("request rejection must terminalize");
    };

    assert_eq!(
        failure.message,
        "provider returned WebSocket error 400 (token=example-value\\u{000A}\\u{0009}\\u{001B}[2J\\u{202E}done)"
    );
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
}

/// SSE marks dispatch once before the fake server can receive request bytes, so
/// local request construction and capture work cannot enter semantic latency.
#[test]
fn sse_dispatch_callback_precedes_request_bytes_and_runs_once() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking fake server");
    let (callback_started_tx, callback_started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let callback_count = Arc::new(AtomicUsize::new(0));
    let client_count = Arc::clone(&callback_count);
    let client = std::thread::spawn(move || {
        run_attempt_with_debug(
            &minimal_prompt(),
            &AttemptConfig {
                base_url: format!("http://{address}"),
                api_key: String::new(),
                max_output_tokens: 0,
                transport: Transport::Sse,
                prompt_cache: None,
            },
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
            false,
            &mut |update| {
                if matches!(update, AttemptUpdate::Dispatched(_)) {
                    client_count.fetch_add(1, Ordering::SeqCst);
                    callback_started_tx
                        .send(())
                        .expect("notify fake server of dispatch");
                    release_rx
                        .recv_timeout(Duration::from_secs(3))
                        .expect("release request send");
                }
            },
            &mut || false,
            &test_network(),
        )
    });
    callback_started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("dispatch callback must run before request polling");
    assert_eq!(
        listener
            .accept()
            .expect_err("SSE peer must not receive a connection before callback release")
            .kind(),
        ErrorKind::WouldBlock
    );
    release_tx.send(()).expect("release request send");
    listener
        .set_nonblocking(false)
        .expect("restore blocking fake server");
    let (mut socket, _) = listener.accept().expect("accept released SSE request");
    let _ = read_http_request(&mut socket);
    drop(socket);
    let outcome = client.join().expect("join SSE client");
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
}

/// Cancellation raised by SSE request capture must win at the final pre-send
/// check, leaving no dispatch observation.
#[test]
fn sse_capture_cancellation_skips_final_dispatch_observation() {
    let canceled = Arc::new(AtomicBool::new(false));
    let capture_canceled = Arc::clone(&canceled);
    let capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |_| capture_canceled.store(true, Ordering::SeqCst)),
    );
    let callback_count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&callback_count);
    let outcome = run_attempt_with_capture_and_updates(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: "http://example.invalid".to_owned(),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        capture,
        &mut |update| {
            if matches!(update, AttemptUpdate::Dispatched(_)) {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        },
        &mut || canceled.load(Ordering::SeqCst),
        &test_network(),
    );
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert_eq!(callback_count.load(Ordering::SeqCst), 0);
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
        assert_eq!(envelope["prompt_cache_key"], "tau:agent-test");
        assert_eq!(envelope["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(envelope["prompt_cache_options"]["ttl"], "30m");
        assert!(envelope.get("prompt_cache_retention").is_none());
        assert_eq!(
            envelope["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
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
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let debug_capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |capture| captured.lock().expect("capture lock").push(capture)),
    );
    let outcome = run_attempt_with_capture(
        &cache_prefix_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: "test-key".to_owned(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
            prompt_cache: Some(PromptCachePolicy::Explicit),
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        debug_capture,
        &mut |_| {},
        &mut || false,
        &test_network(),
    );
    join_websocket_peer(server);
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("WebSocket Responses attempt must complete");
    };
    assert_eq!(success.provider_response_id.as_deref(), Some("resp_ws"));
    let captures = captures.lock().expect("capture lock");
    assert_eq!(captures.len(), 2);
    assert_eq!(
        captures[0].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketRequest
    );
    let request: Value = serde_json::from_slice(captures[0].json()).expect("request capture");
    assert_eq!(request["body"]["type"], "response.create");
    assert!(request["body"].get("stream").is_none());
    assert_eq!(
        captures[1].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketResponse
    );
}

/// WebSocket connection and upgrade work remain before dispatch, while exactly
/// one callback runs before the fake peer can read `response.create`.
#[test]
fn websocket_dispatch_callback_precedes_response_create_and_runs_once() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let (callback_started_tx, callback_started_rx) = mpsc::channel();
    let (peer_checked_tx, peer_checked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let socket = accept_websocket_peer(&listener);
        let mut socket = tungstenite::accept(socket).expect("upgrade WebSocket");
        callback_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("dispatch callback must run after upgrade");
        socket
            .get_mut()
            .set_nonblocking(true)
            .expect("set nonblocking frame read");
        assert!(
            socket.read().is_err(),
            "response.create must wait for dispatch callback release"
        );
        peer_checked_tx
            .send(())
            .expect("report pre-enqueue frame check");
        socket
            .get_mut()
            .set_nonblocking(false)
            .expect("restore blocking frame read");
        let Message::Text(_) = socket.read().expect("read response.create") else {
            panic!("request must use a text response.create frame");
        };
    });
    let callback_count = Arc::new(AtomicUsize::new(0));
    let client_count = Arc::clone(&callback_count);
    let client = std::thread::spawn(move || {
        run_attempt_with_debug(
            &minimal_prompt(),
            &AttemptConfig {
                base_url: format!("http://{address}"),
                api_key: String::new(),
                max_output_tokens: 0,
                transport: Transport::Websocket,
                prompt_cache: None,
            },
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
            false,
            &mut |update| {
                if matches!(update, AttemptUpdate::Dispatched(_)) {
                    client_count.fetch_add(1, Ordering::SeqCst);
                    callback_started_tx
                        .send(())
                        .expect("notify fake server of dispatch");
                    release_rx
                        .recv_timeout(Duration::from_secs(3))
                        .expect("release response.create");
                }
            },
            &mut || false,
            &test_network(),
        )
    });
    peer_checked_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("fake server must observe callback-held response.create");
    release_tx.send(()).expect("release response.create");
    let outcome = client.join().expect("join WebSocket client");
    join_websocket_peer(server);
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
}

/// Cancellation raised by WebSocket request capture must win after upgrade but
/// before `response.create`, without inventing a dispatch observation or frame.
#[test]
fn websocket_capture_cancellation_skips_final_dispatch_observation() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let server = std::thread::spawn(move || {
        let socket = accept_websocket_peer(&listener);
        let mut socket = tungstenite::accept(socket).expect("upgrade WebSocket");
        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set bounded frame read");
        assert!(
            socket.read().is_err(),
            "canceled response.create must not reach the peer"
        );
    });
    let canceled = Arc::new(AtomicBool::new(false));
    let capture_canceled = Arc::clone(&canceled);
    let capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |_| capture_canceled.store(true, Ordering::SeqCst)),
    );
    let callback_count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&callback_count);
    let outcome = run_attempt_with_capture_and_updates(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        capture,
        &mut |update| {
            if matches!(update, AttemptUpdate::Dispatched(_)) {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        },
        &mut || canceled.load(Ordering::SeqCst),
        &test_network(),
    );
    join_websocket_peer(server);
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert_eq!(callback_count.load(Ordering::SeqCst), 0);
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
        let body =
            r#"{"error":{"code":"invalid_api_key","detail":"DATA:IMAGE/PNG;base64,error-canary"}}"#;
        write!(
            socket,
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write upgrade rejection");
    });
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let debug_capture = DebugCapture::with_test_sink(
        true,
        Arc::new(move |capture| captured.lock().expect("capture lock").push(capture)),
    );
    let trace_output = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace_output = trace_output.clone();
            move || trace_output.clone()
        })
        .finish();
    let outcome = tracing::subscriber::with_default(subscriber, || {
        run_attempt_with_capture(
            &minimal_prompt(),
            &AttemptConfig {
                base_url: format!("http://{address}"),
                api_key: "bad-key".to_owned(),
                max_output_tokens: 0,
                transport: Transport::Websocket,
                prompt_cache: None,
            },
            &AttemptModel {
                id: ModelName::new("test-model"),
            },
            debug_capture,
            &mut |_| {},
            &mut || false,
            &test_network(),
        )
    });
    join_websocket_peer(server);
    let AttemptOutcome::Terminal(failure) = outcome else {
        panic!("authentication rejection must be terminal");
    };
    assert_eq!(
        failure.failure_kind,
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert_eq!(failure.message, "provider returned HTTP 401");
    let trace =
        String::from_utf8(trace_output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert_eq!(
        trace.matches("provider backend stage observation").count(),
        1
    );
    assert!(trace.contains("outcome=\"failed\""), "{trace}");
    assert!(trace.contains("dispatch_count=0"), "{trace}");
    assert!(trace.contains("first_input_seen=false"), "{trace}");
    let captures = captures.lock().expect("capture lock");
    assert_eq!(
        captures.len(),
        1,
        "upgrade failure sends no response.create"
    );
    assert_eq!(
        captures[0].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketResponse
    );
    let error: Value = serde_json::from_slice(captures[0].json()).expect("error capture");
    assert_eq!(error["error"]["body"], "[image data omitted]");
}

/// Binary frames must fail the finite WebSocket attempt before untrusted
/// payloads reach the Responses event parser.
#[test]
fn websocket_rejects_invalid_and_oversized_frames() {
    let outcome = run_websocket_message(Message::Binary(vec![0_u8; 8].into()), &mut || false);
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
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

/// A WebSocket max-output terminal must complete once, preserve a truncated
/// call for inspection, and expose `Length` so the harness suppresses
/// execution.
#[test]
fn websocket_max_output_completes_with_truncated_tool_call() {
    let outcome = run_websocket_messages(
        vec![
            Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"run","arguments":""}}"#
                    .into(),
            ),
            Message::Text(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\""}"#
                    .into(),
            ),
            Message::Text(
                r#"{"type":"response.incomplete","response":{"id":"resp_tool_limited","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":9,"output_tokens":4},"output":[{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"run","arguments":"{\"path\""}]}}"#
                    .into(),
            ),
        ],
        &mut || false,
    );

    let AttemptOutcome::Completed(success) = outcome else {
        panic!("max-output WebSocket attempt must complete without retry");
    };
    assert_eq!(success.stop_reason, ProviderStopReason::Length);
    assert_eq!(
        success.provider_response_id.as_deref(),
        Some("resp_tool_limited")
    );
    assert_eq!(
        success
            .usage
            .expect("terminal usage")
            .response_received_tokens,
        4
    );
    assert!(matches!(
        success.output_items.as_slice(),
        [ContextItem::ToolCall(call)]
            if call.call_id.as_str() == "call_1"
                && call.raw_arguments_json.as_deref() == Some("{\"path\"")
    ));
}

/// A text message exactly at the established one-MiB event limit must still
/// reach the application parser and preserve the inclusive boundary.
#[test]
fn websocket_transport_accepts_exact_limit_message() {
    let event = padded_completed_websocket_event(MAX_EVENT_BYTES);
    let outcome = run_websocket_message(Message::Text(event.into()), &mut || false);

    let AttemptOutcome::Completed(success) = outcome else {
        panic!("exact-limit WebSocket event must complete");
    };
    assert_eq!(success.response_bytes_received, MAX_EVENT_BYTES as u64);
}

/// A single frame one byte above the established event limit must be rejected
/// from its declared length before tungstenite allocates or assembles its
/// payload, so application byte accounting remains untouched.
#[test]
fn websocket_transport_rejects_limit_plus_one_frame_before_application() {
    let outcome = run_websocket_message(
        Message::Text("x".repeat(MAX_EVENT_BYTES + 1).into()),
        &mut || false,
    );

    let AttemptOutcome::Retryable { progress, .. } = outcome else {
        panic!("oversized WebSocket frame must fail retryably");
    };
    assert_eq!(progress.response_bytes_received, 0);
}

/// Individually permitted fragments whose aggregate crosses one MiB must be
/// rejected by tungstenite's message assembler before the application sees a
/// partially or fully assembled event.
#[test]
fn websocket_transport_rejects_fragmented_aggregate_before_application() {
    let text = "x".repeat(MAX_EVENT_BYTES + 1);
    let split = MAX_EVENT_BYTES / 2;
    let outcome =
        run_websocket_messages(fragmented_websocket_text(&text, split, false), &mut || {
            false
        });

    let AttemptOutcome::Retryable { progress, .. } = outcome else {
        panic!("oversized fragmented WebSocket message must fail retryably");
    };
    assert_eq!(progress.response_bytes_received, 0);
}

/// A control frame interleaved between fragments must not count toward the
/// message limit or disturb assembly of an exact-limit text message.
#[test]
fn websocket_transport_preserves_control_frames_during_fragment_assembly() {
    let event = padded_completed_websocket_event(MAX_EVENT_BYTES);
    let outcome = run_websocket_messages(
        fragmented_websocket_text(&event, MAX_EVENT_BYTES / 2, true),
        &mut || false,
    );

    let AttemptOutcome::Completed(success) = outcome else {
        panic!("interleaved ping must preserve exact-limit fragmented event");
    };
    assert_eq!(success.response_bytes_received, MAX_EVENT_BYTES as u64);
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
    let (outcome, captures) =
        run_websocket_messages_captured(vec![Message::Ping(Vec::new().into())], &mut || {
            polls = polls.saturating_add(1);
            8 <= polls
        });
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(captures.len(), 1, "cancellation adds no response artifact");
    assert_eq!(
        captures[0].class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketRequest
    );
}

fn run_websocket_message(
    message: Message,
    is_canceled: &mut impl FnMut() -> bool,
) -> AttemptOutcome {
    run_websocket_messages(vec![message], is_canceled)
}

fn padded_completed_websocket_event(target_len: usize) -> String {
    let template = r#"{"type":"response.completed","padding":"","response":{"id":"resp_1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#;
    assert!(template.len() <= target_len);
    template.replacen(
        r#""padding":"""#,
        &format!(r#""padding":"{}""#, "x".repeat(target_len - template.len())),
        1,
    )
}

fn fragmented_websocket_text(text: &str, split: usize, interleave_ping: bool) -> Vec<Message> {
    assert!(split < text.len());
    let mut messages = vec![Message::Frame(Frame::message(
        text.as_bytes()[..split].to_vec(),
        OpCode::Data(WebSocketData::Text),
        false,
    ))];
    if interleave_ping {
        messages.push(Message::Ping(b"bounded-ingress".to_vec().into()));
    }
    messages.push(Message::Frame(Frame::message(
        text.as_bytes()[split..].to_vec(),
        OpCode::Data(WebSocketData::Continue),
        true,
    )));
    messages
}

fn run_websocket_messages(
    messages: Vec<Message>,
    is_canceled: &mut impl FnMut() -> bool,
) -> AttemptOutcome {
    run_websocket_messages_captured(messages, is_canceled).0
}

fn run_websocket_messages_captured(
    messages: Vec<Message>,
    is_canceled: &mut impl FnMut() -> bool,
) -> (
    AttemptOutcome,
    Vec<tau_provider::debug_capture_writer::ProviderDebugCapture>,
) {
    let stall = matches!(messages.as_slice(), [Message::Ping(_)]);
    let expects_pong = messages
        .iter()
        .any(|message| matches!(message, Message::Ping(_)));
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
        if expects_pong {
            assert!(matches!(socket.read(), Ok(Message::Pong(_))));
        }
        if stall {
            let _ = socket.read();
        }
    });
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    let outcome = run_attempt_with_capture(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        DebugCapture::with_test_sink(
            true,
            Arc::new(move |capture| captured.lock().expect("capture lock").push(capture)),
        ),
        &mut |_| {},
        is_canceled,
        &test_network(),
    );
    join_websocket_peer(server);
    let captures = std::mem::take(&mut *captures.lock().expect("capture lock"));
    (outcome, captures)
}

/// Ensures the real WebSocket callback path has the same borrowed suppressed
/// and due sampling behavior as SSE; only durable terminal output materializes.
#[test]
fn websocket_suppressed_progress_samples_do_not_materialize_display_slots() {
    PROGRESS_MATERIALIZATIONS.with(|count| count.set(0));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind WebSocket server");
    let address = listener.local_addr().expect("WebSocket server address");
    let server = std::thread::spawn(move || {
        let socket = accept_websocket_peer(&listener);
        let mut socket = tungstenite::accept(socket).expect("upgrade WebSocket");
        let _ = socket.read().expect("read response.create");
        socket
            .send(Message::Text(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#
                    .into(),
            ))
            .expect("send item");
        for index in 0..1_000 {
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"type":"response.output_text.delta","output_index":0,"delta":"{index},"}}"#
                    )
                    .into(),
                ))
                .expect("send delta");
        }
        let output = (0..1_000)
            .map(|index| format!("{index},"))
            .collect::<String>();
        socket
            .send(Message::Text(
                format!(
                    r#"{{"type":"response.completed","response":{{"output":[{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{output}"}}]}}]}}}}"#
                )
                .into(),
            ))
            .expect("send terminal");
    });
    let mut due_samples = 0;
    let outcome = run_attempt_with_debug(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Websocket,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        false,
        &mut |update| {
            let AttemptUpdate::Progress(progress) = update else {
                return;
            };
            std::hint::black_box(progress.response_bytes_received());
            std::hint::black_box(progress.has_timed_semantic_output());
            if due_samples == 0 && progress.has_timed_semantic_output() {
                let mut displays = 0;
                progress.visit_display_output(|output| {
                    displays += 1;
                    std::hint::black_box(output.text);
                });
                assert_eq!(displays, 1);
                due_samples += 1;
            }
        },
        &mut || false,
        &test_network(),
    );
    join_websocket_peer(server);
    assert!(matches!(outcome, AttemptOutcome::Completed(_)));
    assert_eq!(due_samples, 1);
    assert_eq!(
        PROGRESS_MATERIALIZATIONS.with(std::cell::Cell::get),
        1,
        "only the separate durable terminal may materialize"
    );
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
                prompt_cache: None,
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
            prompt_cache: None,
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
        read_capped_error_body(
            response,
            &url,
            &mut || false,
            &network,
            deadlines::StreamDeadlines::new(Instant::now()),
        )
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
            prompt_cache: None,
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

/// Complete lines must apply in wire order so a later line whose lossy UTF-8
/// projection exceeds the cap cannot erase an earlier semantic progress update.
#[test]
fn sse_later_lossy_over_limit_line_preserves_earlier_progress() {
    let mut body =
        br#"data: {"type":"response.output_text.delta","output_index":0,"delta":"accepted"}
"#
        .to_vec();
    body.extend(std::iter::repeat_n(b'\xff', MAX_EVENT_BYTES / 3 + 1));
    body.push(b'\n');
    assert!(body.len() < MAX_EVENT_BYTES);
    let mut pending = SseLineBuffer::default();
    pending.append(&body);
    let mut state = State::default();
    let mut update_count = 0;

    let error = process_complete_sse_lines(&mut pending, |line| {
        if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
            state.apply_event(data)?;
            update_count += 1;
        }
        Ok(SseLineControl::Continue)
    })
    .expect_err("later lossy-over-limit line must reject the parser pass");

    assert!(matches!(error, Error::StreamFailure));
    assert_eq!(update_count, 1);
    let progress = state.progress();
    assert!(progress.has_timed_semantic_output);
    assert!(matches!(
        progress.output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "accepted".to_owned(),
        }]
    ));
}

/// Cancellation that arrives while one decoded SSE chunk is being parsed must
/// stop before the next complete line mutates semantic state.
#[test]
fn sse_chunk_processing_observes_cancellation_between_complete_lines() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE server");
    let address = listener.local_addr().expect("SSE server address");
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"accepted\"}\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"rejected\"}\n",
        "data: [DONE]\n",
    );
    let body_len = body.len() as u64;
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        let _ = read_http_request(&mut socket);
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write response");
    });
    let update_count = Cell::new(0);
    let outcome = run_attempt(
        &minimal_prompt(),
        &AttemptConfig {
            base_url: format!("http://{address}"),
            api_key: String::new(),
            max_output_tokens: 0,
            transport: Transport::Sse,
            prompt_cache: None,
        },
        &AttemptModel {
            id: ModelName::new("test-model"),
        },
        &mut |_| update_count.set(update_count.get() + 1),
        &mut || update_count.get() == 1,
        &test_network(),
    );
    server.join().expect("join SSE server");

    let AttemptOutcome::Canceled { progress } = outcome else {
        panic!("cancellation between complete SSE lines must win");
    };
    assert_eq!(update_count.get(), 1);
    assert_eq!(
        progress.response_bytes_received, body_len,
        "cancellation must occur while parsing one fully accounted response chunk"
    );
    assert!(matches!(
        progress.output_items.as_slice(),
        [AttemptOutputItem {
            item: ContextItem::Message(message),
            ..
        }] if message.content == vec![ContentPart::Text {
            text: "accepted".to_owned(),
        }]
    ));
}

/// The residual SSE-line bound accepts exactly one MiB before any newline
/// arrives, preserving the existing inclusive boundary.
#[test]
fn sse_line_buffer_accepts_exact_limit_residual() {
    let mut pending = SseLineBuffer::default();
    pending.append(&vec![b'x'; MAX_EVENT_BYTES]);

    assert!(pending.next_complete_line().is_none());
    pending
        .validate_residual()
        .expect("exact-limit residual must be accepted");

    assert_eq!(pending.consumed(), 0);
}

/// One byte beyond the residual SSE-line bound must fail before another chunk
/// can grow the unterminated provider-controlled line.
#[test]
fn sse_line_buffer_rejects_residual_over_limit() {
    let mut pending = SseLineBuffer::default();
    pending.append(&vec![b'x'; MAX_EVENT_BYTES + 1]);

    assert!(pending.next_complete_line().is_none());
    let error = pending
        .validate_residual()
        .expect_err("cap-plus-one residual must be rejected");

    assert!(matches!(error, Error::StreamFailure));
}

/// Complete-line validation excludes CR/LF framing and applies the existing
/// limit to every newline-terminated line.
#[test]
fn sse_line_buffer_enforces_complete_line_limit() {
    let mut exact = SseLineBuffer::default();
    exact.append(&vec![b'x'; MAX_EVENT_BYTES]);
    exact.append(b"\r\n");
    let mut exact_lines = 0;
    process_complete_sse_lines(&mut exact, |_| {
        exact_lines += 1;
        Ok(SseLineControl::Continue)
    })
    .expect("exact-limit complete line must be accepted");
    assert_eq!(exact_lines, 1);

    let mut oversized = SseLineBuffer::default();
    oversized.append(&vec![b'x'; MAX_EVENT_BYTES + 1]);
    oversized.append(b"\n");
    let error = process_complete_sse_lines(&mut oversized, |_| Ok(SseLineControl::Continue))
        .expect_err("cap-plus-one complete line must be rejected");
    assert!(matches!(error, Error::StreamFailure));
}

/// Aggregate buffered bytes may exceed the per-line cap when every complete
/// line remains independently valid.
#[test]
fn sse_line_buffer_accepts_many_valid_aggregate_lines_over_limit() {
    let line = b": keepalive\n";
    let line_count = MAX_EVENT_BYTES / line.len() + 1;
    let bytes = line.repeat(line_count);
    assert!(MAX_EVENT_BYTES < bytes.len());
    let mut pending = SseLineBuffer::default();
    pending.append(&bytes);

    let mut parsed = 0;
    process_complete_sse_lines(&mut pending, |_| {
        parsed += 1;
        Ok(SseLineControl::Continue)
    })
    .expect("many independently valid lines must be accepted");

    assert_eq!(parsed, line_count);
    assert!(pending.bytes().is_empty());
}

/// Byte-at-a-time appends inspect each byte exactly once, and doubling the
/// workload doubles newline-discovery work instead of producing quadratic work.
#[test]
fn sse_line_buffer_cursor_work_scales_linearly() {
    fn inspected_for(payload_len: usize) -> usize {
        let mut pending = SseLineBuffer::default();
        for byte in std::iter::repeat_n(b'x', payload_len).chain(std::iter::once(b'\n')) {
            pending.append(&[byte]);
            while pending.next_complete_line().is_some() {}
            pending
                .validate_residual()
                .expect("sub-limit byte-at-a-time line must be accepted");
            pending.compact();
        }
        pending.inspected_bytes()
    }

    let small = inspected_for(4 * 1024);
    let large = inspected_for(8 * 1024);
    assert_eq!(small, 4 * 1024 + 1);
    assert_eq!(large, 8 * 1024 + 1);
    assert_eq!(large - 1, 2 * (small - 1));

    fn inspected_blank_lines(line_count: usize) -> usize {
        let mut pending = SseLineBuffer::default();
        pending.append(&vec![b'\n'; line_count]);
        let mut parsed = 0;
        while pending.next_complete_line().is_some() {
            parsed += 1;
        }
        assert_eq!(parsed, line_count);
        pending.inspected_bytes()
    }

    let small_lines = inspected_blank_lines(4 * 1024);
    let large_lines = inspected_blank_lines(8 * 1024);
    assert_eq!(small_lines, 4 * 1024);
    assert_eq!(large_lines, 2 * small_lines);
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
        hosted_tools: Vec::new(),
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
        prompt_cache: None,
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

/// A harness-shaped public Responses compaction prompt must remove only its
/// provider-native trigger, preserve the actual ordinary lowered request, and
/// append the local summary instruction as the final input item.
#[test]
fn local_compaction_request_extends_the_actual_ordinary_wire_prefix() {
    let config = AttemptConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: String::new(),
        max_output_tokens: 4096,
        transport: Transport::Sse,
        prompt_cache: Some(PromptCachePolicy::Explicit),
    };
    let model = AttemptModel {
        id: ModelName::new("test-model"),
    };
    let ordinary_prompt = cache_prefix_prompt();
    let ordinary = serde_json::to_value(
        build_request(&ordinary_prompt, &config, &model).expect("ordinary request"),
    )
    .expect("ordinary JSON");

    let mut compact_prompt = ordinary_prompt.clone();
    compact_prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    compact_prompt
        .context
        .blocks
        .push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            },
        ));
    tau_provider::local_summary_compaction::replace_trailing_trigger(&mut compact_prompt.context)
        .expect("harness trigger");
    let compact = serde_json::to_value(
        build_request(&compact_prompt, &config, &model).expect("compact request"),
    )
    .expect("compact JSON");

    for field in [
        "model",
        "stream",
        "reasoning",
        "instructions",
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_options",
        "max_output_tokens",
        "tools",
        "tool_choice",
    ] {
        assert_eq!(compact.get(field), ordinary.get(field), "{field}");
    }
    let ordinary_input = ordinary["input"].as_array().expect("ordinary input");
    let compact_input = compact["input"].as_array().expect("compact input");
    assert_eq!(
        &compact_input[..ordinary_input.len()],
        ordinary_input,
        "warmed input prefix must be byte-identical"
    );
    assert_eq!(
        compact_input.last(),
        Some(&serde_json::json!({
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": tau_provider::local_summary_compaction::REQUEST,
            }],
        }))
    );
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
        prompt_cache: None,
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
                    presentation: Default::default(),
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

/// Public Responses must lower a typed synthetic compaction summary as the
/// exact accepted bytes under the existing provider-facing user role.
#[test]
fn synthetic_compaction_summary_lowers_as_exact_user_message() {
    let item = ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::SyntheticCompactionSummary {
            text: "exact <summary> bytes".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    let lowered = lower_item(&item)
        .expect("summary is supported")
        .expect("summary is nonempty");

    assert_eq!(
        serde_json::to_value(lowered).expect("wire JSON"),
        serde_json::json!({
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "exact <summary> bytes",
            }],
        })
    );
}

/// Large prompt blocks must be traversed by borrow rather than cloning their
/// complete text payloads before provider lowering. The two sizes make this a
/// descriptive 1 MiB/8 MiB copy-amplification benchmark without allocator-
/// specific assertions.
#[test]
fn borrowed_context_traversal_retains_large_payload_allocations() {
    const MESSAGE_BYTES: usize = 8 * 1024 * 1024;
    const TOOL_BYTES: usize = 1024 * 1024;

    let message_text = "m".repeat(MESSAGE_BYTES);
    let tool_text = "t".repeat(TOOL_BYTES);
    let context = tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text { text: message_text }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: tau_proto::ToolCallId::new("large-result"),
                    tool_type: ToolType::Function,
                    status: ToolResultStatus::Success,
                    output: tau_proto::ToolResponse {
                        raw: tau_proto::CborValue::Text(String::new()),
                        headers: Vec::new(),
                        body: tool_text,
                    },
                    provider_content: Vec::new(),
                }],
            }),
        ],
    };
    let tau_proto::ContextBlock::UserInput(message_block) = &context.blocks[0] else {
        panic!("message block fixture")
    };
    let ContextItem::Message(message) = &message_block.items[0] else {
        panic!("message fixture")
    };
    let ContentPart::Text { text: message_text } = &message.content[0] else {
        panic!("message text fixture")
    };
    let original_message = message_text.as_ptr();
    let tau_proto::ContextBlock::ToolResults(tool_block) = &context.blocks[1] else {
        panic!("tool-result fixture")
    };
    let original_tool = tool_block.items[0].output.body.as_ptr();

    let borrowed = borrowed_context_items(&context).collect::<Vec<_>>();
    let BorrowedContextItem::Context(ContextItem::Message(message)) = borrowed[0] else {
        panic!("borrowed message")
    };
    let BorrowedContextItem::ToolResult(result) = borrowed[1] else {
        panic!("borrowed tool result")
    };

    let ContentPart::Text { text: message_text } = &message.content[0] else {
        panic!("borrowed message text")
    };
    assert_eq!(message_text.as_ptr(), original_message);
    assert_eq!(result.output.body.as_ptr(), original_tool);
    assert_eq!(message_text.len(), MESSAGE_BYTES);
    assert_eq!(result.output.body.len(), TOOL_BYTES);
}
