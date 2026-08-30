use std::sync::atomic as path_std_sync_atomic;
use std::{
    collections as path_std_collections, net as path_std_net, sync as path_std_sync,
    time as path_std_time,
};

use tokio::sync as path_tokio_sync;

use crate::common::LlmError;
use crate::{attempt_failure as path_crate_attempt_failure, common as path_crate_common};

mod compatibility;
mod output_index;

use tau_proto::{
    ContentPart, ContextItem, ContextRole, ImageContent, ImageDetail, ImageMediaType, MessageItem,
    OpaqueProviderItem, ResponsesToolCallEnvelope, ToolCallItem, ToolResultContentPart,
    ToolResultItem, ToolResultStatus,
};

use super::codex_response_wake_generation::CodexResponseWakeGeneration;
use super::*;

/// Shared trace writer for production-callpath assertions.
#[derive(Clone, Default)]
struct TraceWriter(path_std_sync::Arc<path_std_sync::Mutex<Vec<u8>>>);

impl std::io::Write for TraceWriter {
    /// Append one formatted trace fragment.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// The in-memory sink has no external buffer.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An already-canceled unary compact request emits one canceled observation
/// without dispatching or constructing worker-owned trace state.
#[test]
fn unary_compact_pre_worker_cancel_emits_private_trace() {
    struct ImmediateAbort;
    impl crate::TurnAbort for ImmediateAbort {
        fn is_aborted(&mut self) -> bool {
            true
        }

        fn register_waker(
            &mut self,
            _waker: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
        ) -> Box<dyn crate::TurnAbortWaker> {
            Box::new(TestAbortWaker)
        }
    }

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
    let result = tracing::subscriber::with_default(subscriber, || {
        responses_compact(
            "ap-pre-worker-cancel",
            &chain_test_config(),
            &basic_prompt_payload(),
            &mut ImmediateAbort,
            path_std_sync::Arc::new(crate::test_network_policy()),
        )
    });
    assert!(matches!(result, Err(LlmError::Canceled)));
    let trace =
        String::from_utf8(output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert_eq!(
        trace.matches("provider backend stage observation").count(),
        1
    );
    assert!(trace.contains("outcome=\"canceled\""), "{trace}");
    assert!(trace.contains("dispatch_count=0"), "{trace}");
}

/// Ensures the compact transport wake authority retains its zero initial value
/// and wrapping overflow policy, so a wake at the scalar maximum remains
/// distinguishable from an unrelated asynchronous generation domain.
#[test]
fn codex_response_wake_generation_starts_at_zero_and_wraps() {
    let mut generation = CodexResponseWakeGeneration::default();
    assert_eq!(generation, CodexResponseWakeGeneration::new(0));

    generation.advance();
    assert_eq!(generation, CodexResponseWakeGeneration::new(1));

    let mut maximum = CodexResponseWakeGeneration::new(u64::MAX);
    maximum.advance();
    assert_eq!(maximum, CodexResponseWakeGeneration::default());
}

/// Private local-compaction envelopes are harness control output and can never
/// become a Codex Responses input item.
#[test]
fn local_compaction_narrative_is_not_provider_lowerable() {
    let item = ContextItem::LocalCompactionNarrative(tau_proto::LocalCompactionNarrativeItem {
        narrative: "private narrative".to_owned(),
    });
    let mut budget = ImageRequestBudget {
        supported: false,
        responses_lite: false,
        image_bytes: 0,
        data_url_bytes: 0,
    };
    let mut lowered = Vec::new();
    convert_context_item(&item, true, &mut budget, &mut lowered);
    assert!(lowered.is_empty());
}

/// Codex Responses must lower a typed synthetic compaction summary as the
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
    let mut budget = ImageRequestBudget {
        supported: false,
        responses_lite: false,
        image_bytes: 0,
        data_url_bytes: 0,
    };
    let mut lowered = Vec::new();
    convert_context_item(&item, true, &mut budget, &mut lowered);

    assert_eq!(
        serde_json::to_value(lowered).expect("wire JSON"),
        serde_json::json!([{
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "exact <summary> bytes",
            }],
        }])
    );
}

/// Private Responses terminal usage keeps provider-reported cache writes
/// separate from cached reads for equivalent-API accounting.
#[test]
fn terminal_usage_parses_cache_write_tokens() {
    let mut state = StreamState::new();
    apply_terminal_event(
        &mut state,
        &serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp-cache-write",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 0,
                    "input_tokens_details": {
                        "cached_tokens": 60,
                        "cache_write_tokens": 30
                    }
                }
            }
        }),
    );

    let usage = state.usage().expect("terminal usage");
    let cache = usage.cache.expect("cache usage");
    assert_eq!(cache.read_tokens, Some(60));
    assert_eq!(cache.write_tokens, Some(30));
}

type AbortCallback = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;
type AbortCallbacks = std::sync::Arc<std::sync::Mutex<Vec<AbortCallback>>>;

/// Abort source that retains every compact cancellation callback for a test.
struct CompactCapturingAbort {
    /// Shared cancellation flag returned by `is_aborted`.
    aborted: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
    /// Registered callbacks invoked by the test driver.
    wakers: AbortCallbacks,
}

impl crate::TurnAbort for CompactCapturingAbort {
    fn is_aborted(&mut self) -> bool {
        self.aborted.load(path_std_sync_atomic::Ordering::SeqCst)
    }

    fn register_waker(&mut self, waker: AbortCallback) -> Box<dyn crate::TurnAbortWaker> {
        self.wakers.lock().expect("abort wakers").push(waker);
        Box::new(TestAbortWaker)
    }
}

/// Unwind-safe release ownership for the compact worker exit barrier.
struct CompactExitRelease {
    /// Sender retained until explicit release or unwind.
    sender: Option<path_std_sync::mpsc::SyncSender<()>>,
}

impl CompactExitRelease {
    /// Releases the held worker exactly once.
    fn release(mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send(()).expect("release compact worker");
        }
    }
}

impl Drop for CompactExitRelease {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(());
        }
    }
}

/// Ensures historical XML-escaped and current exact-close web results remain
/// byte-for-byte intact on the Codex/Responses function-call-output path.
#[test]
fn web_content_envelope_is_preserved_in_responses_tool_result() {
    for envelope in [
        "<tau_web_content adapter=\"exa\" operation=\"search\" content_trust=\"external\">Title: &lt;claim&gt;</tau_web_content>",
        "<tau_web_content adapter=\"exa\" operation=\"search\" content_trust=\"external\">Title: <claim> & &lt;/tau_web_content&gt;</tau_web_content>",
    ] {
        let result = ToolResultItem {
            presentation: Default::default(),
            call_id: "call-web".into(),
            tool_type: tau_proto::ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                envelope.to_owned(),
            )),
            provider_content: Vec::new(),
        };
        let mut budget = ImageRequestBudget {
            supported: false,
            responses_lite: false,
            image_bytes: 0,
            data_url_bytes: 0,
        };
        assert_eq!(
            convert_tool_result_output(&result, &mut budget),
            serde_json::Value::String(envelope.to_owned())
        );
    }
}

/// Golden compatibility guard for the exact ChatGPT Responses function
/// envelope carrying Codex's call-local `workdir` spelling.
#[test]
fn shell_command_wire_definition_uses_only_call_local_workdir() {
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("gpt_shell"),
        model_visible_name: Some(tau_proto::ToolName::new("shell_command")),
        description: Some("Run a shell command.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout": {"type": "integer"},
                "workdir": {"type": "string"}
            },
            "required": ["command"],
            "additionalProperties": false
        })),
        format: None,
    };

    assert_eq!(
        convert_tool_definition(&tool),
        serde_json::json!({
            "type": "function",
            "name": "shell_command",
            "description": "Run a shell command.",
            "strict": null,
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer"},
                    "workdir": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        })
    );
}

/// Ensures both GPT-5.6 modes receive native image function output while only
/// standard Responses preserves the audited high-detail wire field.
#[test]
fn gpt_5_6_lowers_typed_image_inside_function_output() {
    let items = [ContextItem::ToolResult(ToolResultItem {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            "image/png image, 1x1, 12 bytes, high detail".to_owned(),
        )),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: b"\x89PNG\r\n\x1a\nDATA".to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
    })];
    let request = PromptPayload {
        system_prompt: "",
        context: context(&items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    for mode in [ResponsesMode::Standard, ResponsesMode::LiteCompatibility] {
        let mut config = chain_test_config();
        config.model_id = "gpt-5.6-sol".to_owned();
        config.mode = mode;
        let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
        let output = body["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("function output");
        assert_eq!(output["call_id"], "call-image");
        assert_eq!(output["output"][0]["type"], "input_text");
        assert_eq!(output["output"][1]["type"], "input_image");
        assert_eq!(
            output["output"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgpEQVRB"
        );
        if mode == ResponsesMode::Standard {
            assert_eq!(output["output"][1]["detail"], "high");
        } else {
            assert!(
                output["output"][1].get("detail").is_none(),
                "Responses Lite strips detail only after local high-detail preparation"
            );
        }
    }
}

/// Ensures aggregate raw-image and expanded data-URL budgets both omit an image
/// before base64 allocation once the request-wide limit would be crossed.
#[test]
fn typed_image_lowering_enforces_both_request_budgets() {
    let result = ToolResultItem {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            "image metadata".to_owned(),
        )),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: b"\x89PNG\r\n\x1a\nDATA".to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
    };
    for mut budget in [
        ImageRequestBudget {
            supported: true,
            responses_lite: true,
            image_bytes: MAX_REQUEST_IMAGE_BYTES,
            data_url_bytes: 0,
        },
        ImageRequestBudget {
            supported: true,
            responses_lite: true,
            image_bytes: 0,
            data_url_bytes: MAX_REQUEST_IMAGE_DATA_URL_BYTES,
        },
    ] {
        let output = convert_tool_result_output(&result, &mut budget);
        assert!(output.to_string().contains("request limit exceeded"));
        assert!(!output.to_string().contains("data:image"));
    }
}

/// Ensures unaudited Responses routes fail closed by projecting a bounded text
/// placeholder instead of sending image bytes or synthesizing a user message.
#[test]
fn unaudited_responses_route_omits_typed_image() {
    let config = chain_test_config();
    let items = [ContextItem::ToolResult(ToolResultItem {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
            "image metadata".to_owned(),
        )),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: b"\x89PNG\r\n\x1a\nDATA".to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
    })];
    let request = PromptPayload {
        system_prompt: "",
        context: context(&items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let output = &body["input"][0]["output"];
    assert!(
        output
            .as_str()
            .expect("omission marker is text")
            .contains("does not support")
    );
    assert!(!body.to_string().contains("data:image"));
}

/// Ensures provider diagnostics replace data URLs before JSON is persisted.
#[test]
fn provider_debug_redaction_removes_image_data_urls() {
    let mut value = serde_json::json!({
        "image_url": "data:image/png;base64,iVBORw0KGgo="
    });
    redact_image_data_urls(&mut value);
    assert_eq!(
        value["image_url"],
        "<image/png; base64 omitted; 34 encoded bytes; \
         sha256=e1e10747c2374f621aa59fefede6ef99dc6acdb41b267ab4af408d5529f89ea8>"
    );

    let mut same_length_distinct = serde_json::json!({
        "image_url": "data:image/png;base64,iVBORw0KGgp="
    });
    redact_image_data_urls(&mut same_length_distinct);
    assert_ne!(same_length_distinct["image_url"], value["image_url"]);
}

/// Ensures the exact WebSocket request object retained by VCR recording is
/// redacted, not merely the separate replay lookup projection.
#[test]
fn websocket_vcr_recording_redacts_image_data_urls() {
    let envelope = serde_json::json!({
        "type": "response.create",
        "input": [{
            "type": "input_image",
            "image_url": "data:image/png;base64,iVBORw0KGgo="
        }]
    });

    let recorded = ws::recorded_request_body(&envelope, true)
        .expect("serialize recorded request")
        .expect("recording body");
    assert!(!recorded.to_string().contains("data:image"));
    assert_eq!(
        recorded["input"][0]["image_url"],
        "<image/png; base64 omitted; 34 encoded bytes; \
         sha256=e1e10747c2374f621aa59fefede6ef99dc6acdb41b267ab4af408d5529f89ea8>"
    );
}

fn context(items: &[ContextItem]) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: items.to_vec(),
            },
        )],
    }))
}

fn context_with_response_id(
    response_id: &str,
    before: Vec<ContextItem>,
    assistant: Vec<ContextItem>,
    after: Vec<ContextItem>,
) -> &'static tau_proto::PromptContext {
    Box::leak(Box::new(tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: before }),
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: Some(response_id.to_owned()),
                backend: None,
                output_items: assistant,
                usage: None,
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: after }),
        ],
    }))
}

fn response_anchor_from_context(
    context: &tau_proto::PromptContext,
    response_id: &str,
) -> Option<CachedResponseAnchor> {
    let context_items = context.flatten();
    let mut prefix_len = context_items.len();
    for block in context.blocks.iter().rev() {
        match block {
            tau_proto::ContextBlock::AssistantResponse(response) => {
                if response.provider_response_id.as_deref() == Some(response_id) {
                    return CachedResponseAnchor::new(
                        response_id.to_owned(),
                        &context_items[..prefix_len],
                    );
                }
                prefix_len = prefix_len.saturating_sub(response.output_items.len());
            }
            tau_proto::ContextBlock::UserInput(block) => {
                prefix_len = prefix_len.saturating_sub(block.items.len());
            }
            tau_proto::ContextBlock::ToolResults(block) => {
                prefix_len = prefix_len.saturating_sub(block.items.len());
            }
        }
    }
    None
}

fn terminal_vcr_stream(response_id: &str) -> ProviderRawEventStream {
    ProviderRawEventStream {
        raw_events: vec![ProviderRawEvent {
            delta_micros: 0,
            raw: serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "input_tokens_details": {"cached_tokens": 0},
                    },
                },
            })
            .to_string(),
        }],
    }
}

/// VCR replay uses the same one-decode envelope as live WebSocket parsing and
/// retains exact opaque item bytes.
#[test]
fn websocket_replay_decodes_each_event_once_and_preserves_opaque_sidecar() {
    let raw_item = r#"{ "type":"compaction", "summary":{"n":1.2300} }"#;
    let stream = ProviderRawEventStream {
        raw_events: vec![
            ProviderRawEvent {
                delta_micros: 0,
                raw: r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"reset_at":1700000000}}}"#.to_owned(),
            },
            ProviderRawEvent {
                delta_micros: 0,
                raw: format!(
                    r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_item}}}"#
                ),
            },
            ProviderRawEvent {
                delta_micros: 0,
                raw: r#"{"type":"response.completed","response":{"id":"replay-once"}}"#.to_owned(),
            },
        ],
    };
    crate::decoded_event::reset_test_counts();
    let state =
        ws::run_replay(&stream, ws::ResponseMode::Ordinary, &mut |_| {}).expect("replayed stream");
    assert_eq!(crate::decoded_event::test_counts(), (3, 3));
    assert!(state.quota_observation.is_some());
    let Some(OutputItemAccumulator::Compaction(Some(item))) = state.output_items.first() else {
        panic!("opaque compaction output");
    };
    assert_eq!(item.raw_json(), raw_item);
}

#[test]
fn build_request_includes_prompt_cache_key_when_supported() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        raw_context_window: tau_proto::TokenCount::new(258400),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_compaction: false,
        supports_prompt_cache_key: true,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let prompt_cache_key = body["prompt_cache_key"].as_str().expect("prompt_cache_key");

    assert!(uuid::Uuid::parse_str(prompt_cache_key).is_ok());
}

/// Ensures the enabled real Responses request producer submits typed WebSocket
/// request metadata through the shared capture boundary.
#[test]
fn debug_request_producer_submits_typed_compressed_capture_job() {
    let mut correlation =
        path_crate_attempt_failure::AttemptCaptureCorrelation::new(crate::LogicalAttempt::new(7));
    let dispatch = correlation.next_dispatch();
    let config = chain_test_config();
    let session_id = tau_proto::SessionId::parse("session-test").expect("session id");
    let agent_id = tau_proto::AgentId::parse("agent-test").expect("agent id");
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        debug_provider_requests: true,
    };
    let body = serde_json::json!({"input": [{"role": "user"}]});
    let mut submitted = None;

    serialize_and_submit_provider_request_with(
        "prompt-test",
        &config,
        &request,
        tau_proto::ProviderBackendTransport::Websocket,
        Some(dispatch),
        &body,
        |capture| submitted = Some(capture),
    )
    .expect("serialize request capture");

    let capture = submitted.expect("capture submitted");
    assert_eq!(capture.session_id(), &session_id);
    assert_eq!(capture.agent_prompt_id().as_str(), "prompt-test");
    assert_eq!(
        capture.class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketRequest
    );
    let metadata: serde_json::Value = serde_json::from_slice(capture.json()).expect("capture JSON");
    assert_eq!(metadata["backend"], "responses");
    assert_eq!(metadata["transport"], "websocket");
    assert_eq!(metadata["logical_attempt"], 7);
    assert_eq!(metadata["wire_dispatch_index"], 1);
    assert_eq!(metadata["body"], body);

    let mut standalone = None;
    serialize_and_submit_provider_request_with(
        "prompt-test",
        &config,
        &request,
        tau_proto::ProviderBackendTransport::HttpSse,
        None,
        &body,
        |capture| standalone = Some(capture),
    )
    .expect("serialize standalone capture");
    let standalone: serde_json::Value =
        serde_json::from_slice(standalone.expect("standalone capture").json())
            .expect("standalone JSON");
    assert!(standalone.get("logical_attempt").is_none());
    assert!(standalone.get("wire_dispatch_index").is_none());
}

#[test]
fn build_request_includes_service_tier_when_configured() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        raw_context_window: tau_proto::TokenCount::new(258400),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["service_tier"], "priority");
}

/// Tau's `off` is a user-visible request for no reasoning, not "use the
/// provider default". GPT-5.5 defaults omitted effort to `medium`, so the
/// OpenAI provider must send the explicit `none` value when effort is off.
#[test]
fn build_request_maps_off_effort_to_openai_none() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_reasoning_effort: true,
        ..chain_test_config()
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["reasoning"]["effort"], "none");
}

/// Ensures GPT-5.6 maximum effort reaches the Responses API using the
/// provider's exact `max` wire value.
#[test]
fn build_request_maps_max_effort_to_openai_max() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        model_id: "gpt-5.6-sol".into(),
        supports_reasoning_effort: true,
        ..chain_test_config()
    };
    let request = PromptPayload {
        params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Max,
            ..Default::default()
        },
        ..basic_prompt_payload()
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["reasoning"]["effort"], "max");
}

#[test]
fn build_request_omits_prompt_cache_key_without_seed() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        raw_context_window: tau_proto::TokenCount::new(258400),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert!(!object.contains_key("prompt_cache_key"));
}

/// First turn (no chain established): the request must contain the
/// full transcript, `store: false`, and no `previous_response_id`.
/// This is the baseline that future stateful-chain optimizations are
/// compared against; if it ever flips, every turn would start
/// charging for stored responses by accident.
#[test]
fn build_request_first_turn_replays_full_history_without_chain() {
    let config = chain_test_config();
    let messages = vec![user_text("hello"), assistant_text("hi there")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    // Two messages → two `input` items (one user text, one assistant message).
    assert_eq!(
        input.len(),
        2,
        "full history must be replayed when chain is absent"
    );
}

/// Regression: a daemon restore can repair an interrupted foreground tool call
/// by writing a durable synthetic internal tool error before the user sends the
/// next prompt. With no chain anchor after restore, Responses must replay the
/// repaired assistant tool call, its matching output, and the new user message
/// without sending a stale `previous_response_id`.
#[test]
fn build_request_full_replay_serializes_restored_tool_error_before_next_user_message() {
    let config = chain_test_config();
    let messages = vec![
        assistant_tool_call(
            "call-restored",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Map(vec![(
                tau_proto::CborValue::Text("command".to_owned()),
                tau_proto::CborValue::Text("sleep 30".to_owned()),
            )]),
        ),
        restored_internal_tool_error("call-restored", "partial stdout before restart"),
        user_text("after restart"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request body is an object");
    assert!(
        object.get("previous_response_id").is_none(),
        "restored full replay must not send a stale chain id"
    );

    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        3,
        "restored full replay must keep the repaired tool round balanced"
    );
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["id"], "fc_call-restored");
    assert_eq!(input[0]["call_id"], "call-restored");
    assert_eq!(input[0]["arguments"], "{\"command\":\"sleep 30\"}");
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "call-restored");
    let output = input[1]["output"].as_str().expect("tool output");
    assert!(output.contains("error: tau_internal: true"));
    assert!(output.contains("Tool call `call-restored` was interrupted"));
    assert!(output.contains("partial stdout before restart"));
    assert_eq!(input[2]["role"], "user");
    assert_eq!(input[2]["content"][0]["text"], "after restart");
}

/// Responses replay must use the provider's original function-call argument
/// string when it is available. Re-serializing parsed CBOR can reorder keys or
/// normalize whitespace/numbers, which changes the provider-visible cache
/// input.
#[test]
fn build_request_full_replay_preserves_raw_function_call_arguments() {
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    let messages = vec![
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call-raw".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Map(vec![
                (
                    tau_proto::CborValue::Text("z".to_owned()),
                    tau_proto::CborValue::Integer(1.into()),
                ),
                (
                    tau_proto::CborValue::Text("a".to_owned()),
                    tau_proto::CborValue::Array(vec![
                        tau_proto::CborValue::Integer(2.into()),
                        tau_proto::CborValue::Integer(3.into()),
                    ]),
                ),
            ]),
            raw_arguments_json: Some(raw_arguments.to_owned()),
            responses_envelope: None,
        }),
        user_text("continue"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&chain_test_config(), &request, None))
        .expect("serialize request");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["arguments"], raw_arguments);
}

/// Responses replay must keep provider output item ids and envelope fields
/// distinct from semantic tool-result `call_id`s. The provider item `id` is
/// part of full-transcript cache identity, while `call_id` remains Tau's
/// dispatch/result pairing key.
#[test]
fn build_request_full_replay_preserves_responses_tool_call_envelope() {
    let messages = vec![
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call_semantic".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Map(vec![]),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: Some(ResponsesToolCallEnvelope {
                item_id: Some("fc_provider_item".to_owned()),
                status: Some("completed".to_owned()),
                extra_fields: Some(tau_proto::CborValue::Map(vec![(
                    tau_proto::CborValue::Text("provider_future".to_owned()),
                    tau_proto::CborValue::Bool(true),
                )])),
            }),
        }),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call_custom_semantic".into(),
            name: tau_proto::ToolName::new("custom"),
            tool_type: tau_proto::ToolType::Custom,
            arguments: tau_proto::CborValue::Text("raw custom input".to_owned()),
            raw_arguments_json: None,
            responses_envelope: Some(ResponsesToolCallEnvelope {
                item_id: Some("ctc_provider_item".to_owned()),
                status: Some("in_progress".to_owned()),
                extra_fields: None,
            }),
        }),
        user_text("continue"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&chain_test_config(), &request, None))
        .expect("serialize request");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["id"], "fc_provider_item");
    assert_eq!(input[0]["call_id"], "call_semantic");
    assert_eq!(input[0]["status"], "completed");
    assert_eq!(input[0]["provider_future"], true);
    assert_eq!(input[1]["type"], "custom_tool_call");
    assert_eq!(input[1]["id"], "ctc_provider_item");
    assert_eq!(input[1]["call_id"], "call_custom_semantic");
    assert_eq!(input[1]["status"], "in_progress");
}

/// Inference-owned placement supplies `H, R, Q`; the exact anchor at `R`
/// sends only the single post-response `Q` occurrence. When the harness
/// supplies a `previous_response`, the request body slices off the prefix
/// already covered by that response and pins the prior `response.id`.
/// `store` stays `false` — the Codex endpoint *rejects* `store: true`
/// (`HTTP 400 {"detail":"Store must be set to false"}`) even when
/// chaining, in contrast with the public Responses API. Tau today
/// only routes Responses through Codex, so this asserts the Codex
/// shape; a future public-API path would need a separate test.
#[test]
fn build_request_inference_deferred_placement_sends_exact_suffix_and_previous_response_id() {
    let config = chain_test_config();
    // Full transcript: 1 user, 1 assistant response, 1 user.
    // The cached response id was captured after the assistant turn, so only
    // the trailing user message should make it into the request.
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_abc",
            vec![user_text("first turn")],
            vec![assistant_text("first response")],
            vec![user_text("second turn")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let anchor =
        response_anchor_from_context(request.context, "resp_abc").expect("response anchor");
    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");

    assert_eq!(
        body["store"], false,
        "Codex rejects store=true even when chaining"
    );
    assert_eq!(body["previous_response_id"], "resp_abc");
    let input = body["input"].as_array().expect("input array");
    assert_eq!(
        input.len(),
        1,
        "only messages after the anchor should be sent"
    );
    assert_eq!(input[0]["content"][0]["text"], "second turn");
}

/// Canonical CBOR with a non-string map key is valid provider context and must
/// never panic or suppress full replay while an anchor fingerprint is
/// published.
#[test]
fn response_anchor_fingerprint_accepts_non_string_cbor_map_keys() {
    let item = assistant_tool_call(
        "call-array-key",
        "tool",
        tau_proto::ToolType::Function,
        tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Array(vec![tau_proto::CborValue::Integer(1.into())]),
            tau_proto::CborValue::Text("value".to_owned()),
        )]),
    );

    assert!(
        CachedResponseAnchor::new("resp_cbor".to_owned(), &[item]).is_some(),
        "type-preserving CBOR encoding must accept non-string keys",
    );
}

/// Integer and text CBOR map keys that JSON aliases to the same object key must
/// produce distinct response-prefix proofs.
#[test]
fn response_anchor_fingerprint_distinguishes_cbor_map_key_types() {
    let item_with_key = |key| {
        assistant_tool_call(
            "call-key-type",
            "tool",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Map(vec![(key, tau_proto::CborValue::Text("value".to_owned()))]),
        )
    };
    let integer_key = CachedResponseAnchor::new(
        "resp_key".to_owned(),
        &[item_with_key(tau_proto::CborValue::Integer(1.into()))],
    )
    .expect("integer-key fingerprint");
    let text_key = CachedResponseAnchor::new(
        "resp_key".to_owned(),
        &[item_with_key(tau_proto::CborValue::Text("1".to_owned()))],
    )
    .expect("text-key fingerprint");

    assert_ne!(
        integer_key.represented_prefix_fingerprint,
        text_key.represented_prefix_fingerprint,
    );
}

/// The allocation-free streaming encoder must preserve the original
/// count-and-length-framed CBOR identity for every context-item family and for
/// the borrowed tool-result wrapper used by grouped tool blocks.
#[test]
fn response_anchor_streaming_fingerprint_matches_owned_reference_for_all_item_variants() {
    fn reference(items: &[ContextItem]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CachedResponseAnchor::FINGERPRINT_DOMAIN);
        hasher.update(
            &u64::try_from(items.len())
                .expect("fixture item count")
                .to_le_bytes(),
        );
        for item in items {
            let encoded = tau_proto::encode_message_to_vec(item).expect("reference CBOR");
            hasher.update(
                &u64::try_from(encoded.len())
                    .expect("fixture encoded length")
                    .to_le_bytes(),
            );
            hasher.update(&encoded);
        }
        hasher.finalize()
    }

    let tool_result = restored_internal_tool_error("call-result", "tool output");
    let variants = vec![
        user_text("message"),
        assistant_tool_call(
            "call",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Map(vec![(
                tau_proto::CborValue::Integer(7.into()),
                tau_proto::CborValue::Array(vec![tau_proto::CborValue::Text("nested".to_owned())]),
            )]),
        ),
        tool_result.clone(),
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Summary,
            text: "reasoning summary".to_owned(),
        }),
        ContextItem::LocalCompactionNarrative(tau_proto::LocalCompactionNarrativeItem {
            narrative: "private narrative".to_owned(),
        }),
        reasoning_item(r#"{"type":"reasoning","summary":[{"text":"opaque"}]}"#),
        ContextItem::CompactionTrigger,
        ContextItem::Compaction(
            OpaqueProviderItem::from_raw_json(r#"{"type":"compaction","summary":"old"}"#)
                .expect("valid compaction"),
        ),
        ContextItem::UnknownProviderItem(
            OpaqueProviderItem::from_raw_json(r#"{"type":"future_item","deep":[[1,2]]}"#)
                .expect("valid unknown provider item"),
        ),
    ];

    let streamed = CachedResponseAnchor::fingerprint_items(
        variants.len(),
        variants.iter().map(BorrowedContextItem::Context),
    )
    .expect("streamed variant fingerprint");
    assert_eq!(streamed, reference(&variants));

    let ContextItem::ToolResult(tool_result) = &tool_result else {
        panic!("tool-result fixture")
    };
    let context = tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock { items: variants }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![tool_result.clone()],
            }),
        ],
    };
    let owned = context.flatten();
    let streamed =
        CachedResponseAnchor::fingerprint_items(owned.len(), borrowed_context_items(&context))
            .expect("streamed block fingerprint");
    assert_eq!(streamed, reference(&owned));
}

/// A deep warm prefix is hashed exactly once at the item level while request
/// lowering visits only the one-item suffix. This is a deterministic work-count
/// benchmark: growing `PREFIX_ITEMS` must not grow the serialized request
/// input.
#[test]
fn response_anchor_large_prefix_work_is_bounded_to_hash_depth_and_suffix_lowering() {
    const PREFIX_ITEMS: usize = 4_096;

    let mut before = Vec::with_capacity(PREFIX_ITEMS);
    before.extend((0..PREFIX_ITEMS).map(|index| user_text(&format!("prefix-{index}"))));
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_large",
            before,
            vec![assistant_text("anchored response")],
            vec![user_text("one suffix")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("large-prefix").expect("session id"),
        agent_id: &tau_proto::AgentId::parse("large-prefix-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let anchor =
        response_anchor_from_context(request.context, "resp_large").expect("response anchor");
    let fingerprint_visits = path_std_sync::Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let observed_visits = path_std_sync::Arc::clone(&fingerprint_visits);
    let body = with_fingerprint_item_observer(
        move |_| {
            observed_visits.fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
        },
        || serde_json::to_value(build_request(&chain_test_config(), &request, Some(&anchor))),
    )
    .expect("serialize request");

    assert_eq!(
        fingerprint_visits.load(path_std_sync_atomic::Ordering::Relaxed),
        PREFIX_ITEMS + 1,
        "the anchor prefix contains every prior input plus the assistant response",
    );
    let input = body["input"].as_array().expect("request input");
    assert_eq!(input.len(), 1, "only the selected suffix is lowered");
    assert_eq!(input[0]["content"][0]["text"], "one suffix");
}

/// A response containing provider compaction output must remain ineligible as a
/// chain anchor even when its causal-prefix fingerprint matches. This preserves
/// the established workaround for Codex rebuilding compaction history wrongly.
#[test]
fn build_request_compaction_response_anchor_falls_back_to_full_replay() {
    let config = chain_test_config();
    let compaction = ContextItem::Compaction(
        OpaqueProviderItem::from_raw_json(r#"{"type":"compaction","summary":"older context"}"#)
            .expect("valid compaction item"),
    );
    let request = PromptPayload {
        system_prompt: "sys",
        context: Box::leak(Box::new(tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![user_text("first turn")],
                }),
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: Some("resp_compacted".to_owned()),
                    backend: None,
                    output_items: vec![compaction],
                    usage: None,
                }),
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![user_text("after compaction")],
                }),
            ],
        })),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let anchor = response_anchor_from_context(request.context, "resp_compacted")
        .expect("compaction response anchor");

    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(
        body["input"].as_array().map(Vec::len),
        Some(3),
        "compaction response must force the complete transcript onto the wire",
    );
}

/// Defensive: a cached response id missing from the prompt context must NOT
/// chain — fall back to a full-replay first-turn-style request so the
/// conversation keeps working instead of sending an invalid delta.
#[test]
fn build_request_cached_response_missing_from_context_falls_back_to_full_replay() {
    let config = chain_test_config();
    let messages = vec![user_text("only")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let anchor = CachedResponseAnchor::new("missing".to_owned(), &[]).expect("empty prefix");
    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");

    assert_eq!(body["store"], false);
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("previous_response_id")
            .is_none()
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
}

/// Replay-only matching must accept a recorded full request for `C, A, R` even
/// though the transcript alone could also fabricate a chained response anchor.
#[test]
fn websocket_vcr_replays_recorded_causal_mismatch_full_request() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: Box::leak(Box::new(tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![user_text("C")],
                }),
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![user_text("A committed in flight")],
                }),
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: Some("resp_old".to_owned()),
                    backend: None,
                    output_items: vec![assistant_text("R")],
                    usage: None,
                }),
            ],
        })),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("vcr-causal-full")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let agent_prompt_id = "prompt-causal-full";
    let recorded_request = serde_json::to_value(build_ws_envelope(&config, &request, None, None))
        .expect("full request");
    let temp = tempfile::tempdir().expect("VCR tempdir");
    let vcr = tau_vcr::VcrConfig::new(tau_vcr::VcrMode::ReplayOnly, temp.path());
    let key = provider_vcr_key(
        &request,
        agent_prompt_id,
        tau_proto::ProviderBackendTransport::Websocket,
    );
    vcr.store()
        .put(
            &key,
            &ProviderStreamCassette {
                version: PROVIDER_STREAM_CASSETTE_VERSION,
                request: recorded_request,
                stream: terminal_vcr_stream("resp_replayed_full"),
            },
        )
        .expect("store full-request cassette");

    let state = ws::run_vcr_replay_turn(
        &vcr,
        &config,
        agent_prompt_id,
        &request,
        ws::ResponseMode::Ordinary,
        &mut |_| {},
    )
    .expect("replay full request")
    .expect("matching cassette");
    assert_eq!(state.response_id.as_deref(), Some("resp_replayed_full"));
}

/// Replay-only matching must retain an ordinary recorded `C, R, suffix`
/// `previous_response_id` delta rather than requiring the full-request
/// candidate.
#[test]
fn websocket_vcr_replays_recorded_compatible_chained_request() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_old",
            vec![user_text("C")],
            vec![assistant_text("R")],
            vec![user_text("suffix")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("vcr-compatible-chain")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let anchor =
        response_anchor_from_context(request.context, "resp_old").expect("response anchor");
    let agent_prompt_id = "prompt-compatible-chain";
    let recorded_request =
        serde_json::to_value(build_ws_envelope(&config, &request, Some(&anchor), None))
            .expect("chained request");
    let temp = tempfile::tempdir().expect("VCR tempdir");
    let vcr = tau_vcr::VcrConfig::new(tau_vcr::VcrMode::ReplayOnly, temp.path());
    let key = provider_vcr_key(
        &request,
        agent_prompt_id,
        tau_proto::ProviderBackendTransport::Websocket,
    );
    vcr.store()
        .put(
            &key,
            &ProviderStreamCassette {
                version: PROVIDER_STREAM_CASSETTE_VERSION,
                request: recorded_request,
                stream: terminal_vcr_stream("resp_replayed_chain"),
            },
        )
        .expect("store chained-request cassette");

    let state = ws::run_vcr_replay_turn(
        &vcr,
        &config,
        agent_prompt_id,
        &request,
        ws::ResponseMode::Ordinary,
        &mut |_| {},
    )
    .expect("replay chained request")
    .expect("matching cassette");
    assert_eq!(state.response_id.as_deref(), Some("resp_replayed_chain"));
}

/// Regression: `prompt_cache_key` must still ride along on chained
/// (`previous_response_id`) turns. Without it the Codex backend would
/// route the chain continuation to a different machine on each turn
/// and squander the warm cache the chain is supposed to preserve.
#[test]
fn build_request_chain_turn_still_emits_prompt_cache_key() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: context_with_response_id(
            "resp_abc",
            vec![user_text("first turn")],
            vec![assistant_text("first response")],
            vec![user_text("second turn")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let anchor =
        response_anchor_from_context(request.context, "resp_abc").expect("response anchor");
    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");
    assert_eq!(body["previous_response_id"], "resp_abc");
    assert!(body["prompt_cache_key"].is_string());
}

/// The Responses backend must keep the wire `prompt_cache_key` stable for the
/// same target agent even when prompt provenance changes. This pins the
/// regression where manager-relayed sub-agent messages changed originator and
/// cold-started the provider cache bucket.
#[test]
fn build_request_prompt_cache_key_ignores_originator() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("__harness__")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "delegate-1".into(),
    };
    let user_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &ext,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let user_body =
        serde_json::to_value(build_request(&config, &user_request, None)).expect("serialize");
    let ext_body =
        serde_json::to_value(build_request(&config, &ext_request, None)).expect("serialize");

    assert!(user_body["prompt_cache_key"].is_string());
    assert!(ext_body["prompt_cache_key"].is_string());
    assert_eq!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
}

/// The legacy `share_user_cache_key` flag should no longer be needed to make an
/// extension-originated prompt use the stable per-agent cache key. Keeping this
/// no-op avoids letting the flag reintroduce a provenance-derived bucket split.
#[test]
fn build_request_share_user_cache_key_does_not_change_agent_bucket() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("std-notifications")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "idle-0".into(),
    };
    let shared_request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &ext,
        share_user_cache_key: true,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    let body =
        serde_json::to_value(build_request(&config, &shared_request, None)).expect("serialize");
    let default_request = PromptPayload {
        share_user_cache_key: false,
        debug_provider_requests: false,
        ..shared_request
    };
    let default_body =
        serde_json::to_value(build_request(&config, &default_request, None)).expect("serialize");
    assert!(body["prompt_cache_key"].is_string());
    assert_eq!(body["prompt_cache_key"], default_body["prompt_cache_key"]);
}

#[test]
fn build_request_extension_matches_user_wire_body_for_same_context() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let ext = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("std-notifications")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "idle-0".into(),
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run shell commands".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let user_context = context_with_response_id(
        "resp_parent",
        vec![user_text("parent prompt")],
        vec![assistant_text("parent response")],
        vec![user_text("summarize")],
    );
    let ext_context = context_with_response_id(
        "resp_parent",
        vec![user_text("parent prompt")],
        vec![assistant_text("parent response")],
        vec![user_text("summarize")],
    );
    let user_request = PromptPayload {
        system_prompt: "sys",
        context: user_context,
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };
    let ext_request = PromptPayload {
        system_prompt: "sys",
        context: ext_context,
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &ext,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let user_anchor =
        response_anchor_from_context(user_request.context, "resp_parent").expect("user anchor");
    let ext_anchor =
        response_anchor_from_context(ext_request.context, "resp_parent").expect("extension anchor");
    let user_body = serde_json::to_value(build_request(&config, &user_request, Some(&user_anchor)))
        .expect("serialize");
    let ext_body = serde_json::to_value(build_request(&config, &ext_request, Some(&ext_anchor)))
        .expect("serialize");

    assert_eq!(ext_body, user_body);
    assert_eq!(ext_body["prompt_cache_key"], user_body["prompt_cache_key"]);
    assert_eq!(ext_body["tool_choice"], "auto");
    assert_eq!(ext_body["previous_response_id"], "resp_parent");
}

/// A chained Lite delta must not resend the developer-owned tools/instructions
/// prefix already represented by `previous_response_id`.
#[test]
fn build_request_lite_chain_omits_owned_developer_prefix() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-terra".to_owned(),
        ..chain_test_config()
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context_with_response_id(
            "resp_parent",
            vec![user_text("first")],
            vec![assistant_text("answer")],
            vec![user_text("second")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let anchor =
        response_anchor_from_context(request.context, "resp_parent").expect("response anchor");
    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(body["previous_response_id"], "resp_parent");
    assert!(input.iter().all(|item| item["role"] != "developer"));
    assert!(input.iter().all(|item| item["type"] != "additional_tools"));
    assert_eq!(body["parallel_tool_calls"], false);
}

/// Standalone Lite compaction must use the compact schema while retaining the
/// documented developer tool context without ordinary inference-only fields.
#[test]
fn build_compact_request_uses_lite_schema() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-terra".to_owned(),
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[user_text("compact me"), ContextItem::CompactionTrigger]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: Some(tau_proto::TokenCount::new(10)),
        }),
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = build_compact_request(&config, &request).expect("compact body");
    let object = body.as_object().expect("object");
    for forbidden in [
        "stream",
        "store",
        "tool_choice",
        "tools",
        "parallel_tool_calls",
        "reasoning",
        "text",
        "include",
        "context_management",
        "client_metadata",
    ] {
        assert!(!object.contains_key(forbidden), "unexpected {forbidden}");
    }
    assert_eq!(body["input"][0]["type"], "additional_tools");
    assert_eq!(body["instructions"], "system");
    assert_eq!(body["input"].as_array().expect("input").len(), 2);
    assert_eq!(body["input"][1]["role"], "user");
    assert!(body.get("prompt_cache_key").is_some());
    assert_eq!(body["service_tier"], "priority");
    assert!(
        body["input"]
            .as_array()
            .expect("input")
            .iter()
            .all(|item| item["type"] != "compaction_trigger")
    );
}

/// Standard standalone compaction must lower required tools into compact input
/// and omit ordinary inference-only top-level fields.
#[test]
fn build_compact_request_uses_standard_schema() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        model_id: "gpt-5.6-terra".to_owned(),
        supports_compaction: false,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&[user_text("compact me")]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = build_compact_request(&config, &request).expect("compact body");
    assert_eq!(body["instructions"], "system");
    for forbidden in ["tools", "parallel_tool_calls", "reasoning", "text"] {
        assert!(body.get(forbidden).is_none(), "unexpected {forbidden}");
    }
    assert_eq!(body["input"][0]["type"], "additional_tools");
    assert_eq!(body["input"][0]["tools"][0]["name"], "shell");
    assert_eq!(body["input"][1]["role"], "user");
    assert!(body.get("prompt_cache_key").is_some());
    assert_eq!(body["service_tier"], "priority");
}

/// Compact requests retain a valid documented response chain while omitting
/// ordinary inference-only fields.
#[test]
fn build_compact_request_preserves_previous_response_id() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "system",
        context: context_with_response_id(
            "resp_parent",
            vec![user_text("first")],
            vec![assistant_text("answer")],
            vec![user_text("second")],
        ),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = build_compact_request(&config, &request).expect("compact body");

    assert_eq!(body["previous_response_id"], "resp_parent");
    assert!(body.get("tools").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    assert!(body.get("reasoning").is_none());
    assert!(body.get("text").is_none());
}

/// Standalone Lite compact input must preserve balanced function and custom
/// call/output pairs with exact call ids and transport-specific wire types.
#[test]
fn build_compact_request_serializes_balanced_function_and_custom_rounds() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-terra".to_owned(),
        ..chain_test_config()
    };
    let items = vec![
        assistant_tool_call(
            "call-function",
            "function_tool",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Map(Vec::new()),
        ),
        assistant_tool_call(
            "call-custom",
            "custom_tool",
            tau_proto::ToolType::Custom,
            tau_proto::CborValue::Text("custom input".to_owned()),
        ),
        ContextItem::ToolResult(ToolResultItem {
            presentation: Default::default(),
            call_id: "call-function".into(),
            tool_type: tau_proto::ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                "function output".to_owned(),
            )),
            provider_content: Vec::new(),
        }),
        ContextItem::ToolResult(ToolResultItem {
            presentation: Default::default(),
            call_id: "call-custom".into(),
            tool_type: tau_proto::ToolType::Custom,
            status: ToolResultStatus::Error {
                message: "custom error".to_owned(),
            },
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                "custom output".to_owned(),
            )),
            provider_content: Vec::new(),
        }),
    ];
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        debug_provider_requests: false,
    };

    let body = build_compact_request(&config, &request).expect("compact body");
    let input = body["input"].as_array().expect("input");
    let item_for = |call_id: &str, item_type: &str| {
        input
            .iter()
            .find(|item| item["call_id"] == call_id && item["type"] == item_type)
            .unwrap_or_else(|| panic!("missing {item_type} for {call_id}"))
    };
    assert_eq!(
        item_for("call-function", "function_call")["id"],
        "fc_call-function"
    );
    assert_eq!(
        item_for("call-function", "function_call_output")["output"],
        "function output"
    );
    assert_eq!(
        item_for("call-custom", "custom_tool_call")["id"],
        "ctc_call-custom"
    );
    assert_eq!(
        item_for("call-custom", "custom_tool_call_output")["output"],
        "error: custom error\n\ncustom output"
    );
}

/// Unary compact output must preserve provider order and unknown raw items so
/// the accepted replacement window can be replayed without semantic loss.
#[test]
fn parse_compact_response_preserves_ordered_replacement_items() {
    let output = parse_compact_response(
        r#"{"output":[
          {"type":"reasoning","id":"r1","encrypted_content":"opaque"},
          {"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary"}]},
          {"type":"future_item","payload":{"x":1}}
        ]}"#,
    )
    .expect("compact response");

    assert!(matches!(output[0], ContextItem::Reasoning(_)));
    assert!(matches!(output[1], ContextItem::Message(_)));
    assert!(matches!(output[2], ContextItem::UnknownProviderItem(_)));
}

/// Canonical compact output must retain the opaque provider compaction item
/// exactly so the next request can replay the provider's replacement window.
#[test]
fn parse_compact_response_preserves_canonical_compaction_item() {
    let raw = r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#;
    let output = parse_compact_response(include_str!(
        "../../fixtures/compat/responses-compact-output.json"
    ))
    .expect("compact response");

    let ContextItem::Compaction(item) = &output[0] else {
        panic!("expected provider compaction item");
    };
    assert_eq!(item.raw_json(), raw);
}

/// Empty and structurally incomplete compact windows must fail closed rather
/// than becoming a durable boundary that erases valid history.
#[test]
fn parse_compact_response_rejects_unsafe_windows() {
    assert!(matches!(
        parse_compact_response(r#"{"output":[]}"#),
        Err(LlmError::InvalidResponse(_))
    ));
    assert!(matches!(
        parse_compact_response(
            r#"{"output":[{"type":"function_call","call_id":"dangling","name":"shell","arguments":"{}"}]}"#
        ),
        Err(LlmError::InvalidResponse(_))
    ));
}

/// Compact parsing must preserve exact provider JSON spelling in replay
/// sidecars, including key order and numeric spelling for unknown fields.
#[test]
fn parse_compact_response_preserves_raw_item_spelling() {
    let raw = r#"{"type":"future_item","z":1.2300,"a":{"k":2}}"#;
    let output =
        parse_compact_response(&format!(r#"{{"output":[{raw}]}}"#)).expect("compact output");
    let ContextItem::UnknownProviderItem(item) = &output[0] else {
        panic!("expected unknown provider item");
    };
    assert_eq!(item.raw_json(), raw);
}

fn compact_test_failure_capture(
    config: &ResponsesConfig,
    sink: path_std_sync::Arc<
        path_std_sync::Mutex<Vec<tau_provider::debug_capture_writer::ProviderDebugCapture>>,
    >,
) -> compact_failure_capture::CompactFailureCaptureContext {
    let mut prompt = basic_prompt_payload();
    prompt.debug_provider_requests = true;
    compact_failure_capture::CompactFailureCaptureContext::new(
        "ap-compact-failure",
        config,
        &prompt,
    )
    .with_test_sink(sink)
}

/// The retired compact HTTP compatibility boundary submits one causal private
/// capture before it normalizes an ordinary provider rejection.
#[test]
fn compact_http_rejection_submits_failure_capture() {
    use std::io::{Read as _, Write as _};

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("request");
        let body = br#"{"error":{"code":"rejected","message":"causal detail"}}"#;
        write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nX-Request-Id: request-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("headers");
        stream.write_all(body).expect("body");
    });
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        base_url: format!("http://{address}"),
        ..chain_test_config()
    };
    let sink = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let capture = compact_test_failure_capture(&config, path_std_sync::Arc::clone(&sink));

    let result = compact_http_request(
        &config,
        "thread-failure",
        "{}",
        &crate::test_network_policy(),
        &path_tokio_sync::Notify::new(),
        &capture,
        &mut None,
    );

    assert!(matches!(result, Err(LlmError::HttpStatus(400, _))));
    server.join().expect("server");
    let captures = sink.lock().expect("sink");
    assert_eq!(captures.len(), 1);
    let record: serde_json::Value = serde_json::from_slice(captures[0].json()).expect("capture");
    assert_eq!(record["http"]["status"], 400);
    assert_eq!(record["body"]["complete"], true);
    assert_eq!(record["body"]["parsed_error"]["code"]["utf8"], "rejected");
}

/// Cancellation after non-success headers and a body prefix must return
/// `Canceled` while preserving exactly one incomplete prefix/hash capture.
#[test]
fn compact_http_body_cancellation_submits_incomplete_capture() {
    use std::io::{Read as _, Write as _};

    use sha2::Digest as _;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (prefix_tx, prefix_rx) = path_std_sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = path_std_sync::mpsc::sync_channel(1);
    let prefix = b"partial-causal-prefix";
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("request");
        write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            prefix.len() + 1024
        )
        .expect("headers");
        stream.write_all(prefix).expect("prefix");
        stream.flush().expect("flush");
        prefix_tx.send(()).expect("prefix observed");
        let _ = release_rx.recv_timeout(path_std_time::Duration::from_secs(2));
    });
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        base_url: format!("http://{address}"),
        ..chain_test_config()
    };
    let sink = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let (consumed_tx, consumed_rx) = path_std_sync::mpsc::sync_channel(1);
    let capture = compact_test_failure_capture(&config, path_std_sync::Arc::clone(&sink))
        .with_test_body_chunk_observer(path_std_sync::Arc::new(move || {
            let _ = consumed_tx.try_send(());
        }));
    let cancel = path_std_sync::Arc::new(path_tokio_sync::Notify::new());
    let worker_cancel = path_std_sync::Arc::clone(&cancel);
    let (result_tx, result_rx) = path_std_sync::mpsc::sync_channel(1);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            result_tx
                .send(compact_http_request(
                    &config,
                    "thread-partial",
                    "{}",
                    &crate::test_network_policy(),
                    &worker_cancel,
                    &capture,
                    &mut None,
                ))
                .expect("result");
        });
        prefix_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("prefix");
        consumed_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("client consumed prefix");
        cancel.notify_one();
        assert!(matches!(
            result_rx
                .recv_timeout(path_std_time::Duration::from_secs(1))
                .expect("result"),
            Err(LlmError::Canceled)
        ));
    });
    release_tx.send(()).expect("release");
    server.join().expect("server");
    let captures = sink.lock().expect("sink");
    assert_eq!(captures.len(), 1);
    let record: serde_json::Value = serde_json::from_slice(captures[0].json()).expect("capture");
    assert_eq!(record["body"]["complete"], false);
    assert_eq!(record["body"]["decoded_bytes_received"], prefix.len());
    assert_eq!(
        record["body"]["sha256_decoded_received"],
        format!("{:x}", sha2::Sha256::digest(prefix))
    );
}

/// Unary compaction uses dedicated HTTP JSON framing in both modes and scopes
/// the Lite marker to compatibility mode.
#[test]
fn compact_http_request_uses_mode_specific_transport_contract() {
    use std::io::{Read, Write};

    for mode in [ResponsesMode::Standard, ResponsesMode::LiteCompatibility] {
        let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind capture server");
        let address = listener.local_addr().expect("capture address");
        let captured = path_std_sync::Arc::new(path_std_sync::Mutex::new(String::new()));
        let captured_server = path_std_sync::Arc::clone(&captured);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept compact request");
            stream
                .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
                .expect("read timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).expect("read request");
                request.extend_from_slice(&chunk[..count]);
                if count == 0
                    || request
                        .windows(b"\r\n\r\n".len())
                        .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            *captured_server.lock().expect("capture lock") =
                String::from_utf8(request).expect("UTF-8 request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"output\":[]}",
                )
                .expect("write response");
        });
        let config = ResponsesConfig {
            profile_namespace: "chatgpt".to_owned(),
            mode,
            base_url: format!("http://{address}"),
            model_id: "gpt-5.6-terra".to_owned(),
            account_id: Some("acct-test".to_owned()),
            ..chain_test_config()
        };
        let prompt = basic_prompt_payload();
        let failure_capture =
            compact_failure_capture::CompactFailureCaptureContext::new("ap-test", &config, &prompt);

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
        let body = tracing::subscriber::with_default(subscriber, || {
            let mut trace = private_trace::AttemptTrace::selected(
                private_trace::Backend::Codex,
                private_trace::Transport::HttpUnary,
            );
            let body = compact_http_request(
                &config,
                "thread-test",
                "{}",
                &crate::test_network_policy(),
                &path_tokio_sync::Notify::new(),
                &failure_capture,
                &mut trace,
            )
            .expect("compact response");
            trace
                .take()
                .expect("enabled trace")
                .finish(private_trace::Outcome::Completed);
            body
        });
        assert_eq!(body, r#"{"output":[]}"#);
        let trace =
            String::from_utf8(output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
        assert!(trace.contains("dispatch_count=1"), "{trace}");
        assert!(trace.contains("first_input_seen=true"), "{trace}");
        assert!(trace.contains("outcome=\"completed\""), "{trace}");
        server.join().expect("capture server");
        let request = captured.lock().expect("capture lock").to_ascii_lowercase();
        assert!(request.starts_with("post /codex/responses/compact http/1.1\r\n"));
        assert_eq!(
            request.contains("x-openai-internal-codex-responses-lite: true\r\n"),
            mode.is_lite_compatibility()
        );
        assert!(request.contains("chatgpt-account-id: acct-test\r\n"));
        assert!(request.contains("session-id: thread-test\r\n"));
        assert!(request.contains("thread-id: thread-test\r\n"));
        assert!(request.contains("accept: application/json\r\n"));
        assert!(!request.contains("text/event-stream"));
    }
}

/// Ensures canceling a stalled compact request drops the async reqwest future
/// and closes its socket instead of leaving detached prompt-owned network work.
#[test]
fn compact_http_request_cancellation_closes_active_socket() {
    use std::io::Read;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind compact server");
    let address = listener.local_addr().expect("compact address");
    let (accepted_tx, accepted_rx) = path_std_sync::mpsc::channel();
    let (closed_tx, closed_rx) = path_std_sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept compact request");
        stream
            .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
            .expect("read timeout");
        accepted_tx.send(()).expect("accepted receiver");
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!("canceled compact request retained its socket");
                }
                Err(_) => break,
            }
        }
        closed_tx.send(()).expect("closed receiver");
    });
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        base_url: format!("http://{address}"),
        ..chain_test_config()
    };
    let network = tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::new(),
        None,
    );
    let prompt = basic_prompt_payload();
    let failure_capture =
        compact_failure_capture::CompactFailureCaptureContext::new("ap-test", &config, &prompt);
    let cancel = path_std_sync::Arc::new(path_tokio_sync::Notify::new());
    let worker_cancel = path_std_sync::Arc::clone(&cancel);
    let (result_tx, result_rx) = path_std_sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            result_tx
                .send(compact_http_request(
                    &config,
                    "thread-cancel",
                    "{}",
                    &network,
                    &worker_cancel,
                    &failure_capture,
                    &mut None,
                ))
                .expect("result receiver");
        });
        accepted_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("request reached server");
        cancel.notify_one();
        assert!(matches!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("cancellation result"),
            Err(LlmError::Canceled)
        ));
    });
    closed_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("canceled socket closed");
    server.join().expect("compact server");
}

/// Ensures the public compact owner cannot return cancellation while its
/// spawned HTTP worker is still inside the worker-exit boundary.
#[test]
fn compact_cancellation_joins_worker_before_returning() {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind compact server");
    let address = listener.local_addr().expect("compact address");
    let (accepted_tx, accepted_rx) = path_std_sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept compact request");
        stream
            .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
            .expect("bounded server read");
        accepted_tx.send(()).expect("accepted receiver");
        let mut bytes = [0_u8; 4096];
        loop {
            match stream.read(&mut bytes) {
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
                Err(error) => panic!("joined compact worker retained socket: {error}"),
            }
        }
    });
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        base_url: format!("http://{address}"),
        ..chain_test_config()
    };
    let request = basic_prompt_payload();
    let body = build_compact_request(&config, &request).expect("compact body");
    let network = path_std_sync::Arc::new(tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::new(),
        None,
    ));
    let aborted = path_std_sync::Arc::new(AtomicBool::new(false));
    let wakers = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let mut abort = CompactCapturingAbort {
        aborted: path_std_sync::Arc::clone(&aborted),
        wakers: path_std_sync::Arc::clone(&wakers),
    };
    let (exit_reached_tx, exit_reached_rx) = path_std_sync::mpsc::sync_channel(1);
    let (exit_release_tx, exit_release_rx) = path_std_sync::mpsc::sync_channel(1);
    let exit_gate = CompactWorkerExitGate {
        reached: exit_reached_tx,
        release: exit_release_rx,
    };
    let (result_tx, result_rx) = path_std_sync::mpsc::sync_channel(1);

    std::thread::scope(|scope| {
        let exit_release = CompactExitRelease {
            sender: Some(exit_release_tx),
        };
        scope.spawn(|| {
            let result = send_compact_request_inner(
                "ap-compact-join",
                &config,
                &request,
                &mut abort,
                body,
                network,
                &mut None,
                Some(exit_gate),
            );
            result_tx.send(result).expect("result receiver");
        });
        accepted_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("compact request reached server");
        aborted.store(true, Ordering::SeqCst);
        for waker in wakers.lock().expect("abort wakers").clone() {
            waker();
        }
        exit_reached_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("compact worker reached exit gate");
        assert!(
            matches!(
                result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "compact cancellation returned before worker exit"
        );
        exit_release.release();
        assert!(matches!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("joined cancellation result"),
            Err(LlmError::Canceled)
        ));
    });
    server.join().expect("compact server");
}

/// `ToolChoice::None` emits `tool_choice: "none"` on the Responses
/// body while leaving the `tools` array fully declared for standalone
/// compaction and general callers. Non-tool extension side queries deliberately
/// use this selector and separately suppress ordinary and hosted logical web
/// definitions to prevent provider egress. Verified here on a request that
/// carries real tool definitions.
#[test]
fn build_request_emits_tool_choice_none_while_keeping_tools_declared() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::None,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["tool_choice"], "none");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "tools must stay declared so the cache prefix matches"
    );
}

/// Ensures GPT-5.6 uses the Responses Lite body contract while suppressing
/// incompatible server-side compaction: tools and instructions become developer
/// input items, parallel calls are disabled, and persistent reasoning remains.
#[test]
fn build_request_uses_responses_lite_contract_for_gpt_5_6() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-sol".into(),
        raw_context_window: tau_proto::TokenCount::new(372_000),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_compaction: true,
        ..chain_test_config()
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run a shell command".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system instructions",
        context: context(&[ContextItem::CompactionTrigger, user_text("hello")]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: None,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request object");
    let input = body["input"].as_array().expect("input array");

    assert!(!object.contains_key("instructions"));
    assert!(!object.contains_key("tools"));
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"]["context"], "all_turns");
    assert!(body["reasoning"].get("effort").is_none());
    assert!(body["reasoning"].get("summary").is_none());
    assert!(!object.contains_key("context_management"));
    assert_eq!(input[0]["type"], "additional_tools");
    assert_eq!(input[0]["role"], "developer");
    assert_eq!(input[0]["tools"][0]["name"], "shell");
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[1]["role"], "developer");
    assert_eq!(input[1]["content"][0]["text"], "system instructions");
    assert_eq!(input[2]["role"], "user");
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "compaction_trigger")
    );
}

/// Final compaction sizing must measure the exact fresh full WebSocket
/// envelope, including mode-specific metadata, tools, system instructions,
/// stable cache key, and the native trigger.
#[test]
fn full_ws_compaction_measurement_matches_exact_fresh_wire_envelope() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-sol".into(),
        raw_context_window: tau_proto::TokenCount::new(372_000),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_compaction: true,
        supports_prompt_cache_key: true,
        ..chain_test_config()
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run shell".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let originator = tau_proto::PromptOriginator::User;
    let session_id = tau_proto::SessionId::parse("session-cache").expect("session id");
    let agent_id = tau_proto::AgentId::parse("agent-cache").expect("agent id");
    let request = PromptPayload {
        system_prompt: "stable system",
        context: context(&[user_text("prefix"), ContextItem::CompactionTrigger]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        tool_choice: tau_proto::ToolChoice::None,
        params: tau_proto::ModelParams::default(),
        originator: &originator,
        session_id: &session_id,
        agent_id: &agent_id,
        share_user_cache_key: false,
        compaction: None,
        debug_provider_requests: false,
    };
    let measured = full_ws_request_bytes(&config, &request).expect("measure request");
    let exact = serde_json::to_vec(&build_ws_envelope(&config, &request, None, Some(true)))
        .expect("serialize exact envelope")
        .len() as u64;
    assert_eq!(measured, exact);
    let body = serde_json::to_value(build_ws_envelope(&config, &request, None, Some(true)))
        .expect("serialize envelope");
    assert!(
        body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    assert_eq!(
        body["input"]
            .as_array()
            .expect("input")
            .last()
            .expect("compaction request preserves trigger")["type"],
        "compaction_trigger"
    );
    assert!(body.get("previous_response_id").is_none());
}

/// Standalone compact admission must measure the exact unchained WebSocket
/// frame, which omits the ordinary prewarm `generate` field.
#[test]
fn compact_ws_measurement_matches_exact_dispatch_envelope() {
    let config = chain_test_config();
    let request = basic_prompt_payload();
    let measured = compact_ws_request_bytes(&config, &request).expect("measure compact frame");
    let envelope = build_ws_envelope(&config, &request, None, None);
    let exact = u64::try_from(
        serde_json::to_vec(&envelope)
            .expect("serialize compact frame")
            .len(),
    )
    .expect("compact frame length");
    assert_eq!(measured, exact);
    assert!(
        serde_json::to_value(envelope)
            .expect("serialize compact frame")
            .get("generate")
            .is_none(),
        "compact dispatch must not add the ordinary prewarm marker"
    );
}

/// Ensures the default GPT-5.6 route uses standard Responses lowering and
/// truthfully enables parallel direct tool calls.
#[test]
fn build_request_uses_standard_responses_contract_for_gpt_5_6() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        model_id: "gpt-5.6-sol".into(),
        raw_context_window: tau_proto::TokenCount::new(372_000),
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_compaction: false,
        ..chain_test_config()
    };
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("shell"),
        model_visible_name: None,
        description: Some("run a shell command".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
    };
    let request = PromptPayload {
        system_prompt: "system instructions",
        context: context(&[user_text("hello")]),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: None,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let object = body.as_object().expect("request object");

    assert_eq!(body["instructions"], "system instructions");
    assert_eq!(body["tools"][0]["name"], "shell");
    assert_eq!(body["parallel_tool_calls"], true);
    assert!(body["reasoning"].get("context").is_none());
    assert!(!object.contains_key("context_management"));
    assert!(!object.contains_key("client_metadata"));
    assert_eq!(body["input"][0]["role"], "user");
}

/// Ensures standard WebSocket requests carry truthful identity and routing
/// metadata without accidentally acquiring the Lite marker.
#[test]
fn ws_envelope_omits_responses_lite_metadata_in_standard_mode() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        model_id: "gpt-5.6-luna".into(),
        ..chain_test_config()
    };

    let mut request = basic_prompt_payload();
    request.params.service_tier = Some(tau_proto::ServiceTier::Fast);
    let body =
        serde_json::to_value(build_ws_envelope(&config, &request, None, None)).expect("serialize");

    assert_eq!(body["client_metadata"]["originator"], "tau");
    assert_eq!(
        body["client_metadata"]["x-codex-routing-hint"],
        "model=gpt-5.6-luna;tier=priority"
    );
    assert!(
        body["client_metadata"]
            .get("ws_request_header_x_openai_internal_codex_responses_lite")
            .is_none()
    );
    assert!(
        body["client_metadata"]
            .get("x-codex-installation-id")
            .is_none()
    );
    assert!(body["client_metadata"].get("x-oai-attestation").is_none());
    assert_eq!(body["parallel_tool_calls"], true);
}

/// VCR replay validates the exact mode-specific request body: a matching mode
/// succeeds while the opposite mode fails loudly rather than replaying across
/// protocol surfaces.
#[test]
fn provider_cassette_validation_separates_responses_modes() {
    let mut standard = chain_test_config();
    standard.model_id = "gpt-5.6-sol".into();
    standard.mode = ResponsesMode::Standard;
    let mut lite = standard.clone();
    lite.mode = ResponsesMode::LiteCompatibility;
    let request = basic_prompt_payload();
    let standard_body =
        serde_json::to_value(build_request(&standard, &request, None)).expect("standard body");
    let lite_body = serde_json::to_value(build_request(&lite, &request, None)).expect("Lite body");
    let mut stream = ProviderRawEventStream::default();
    record_provider_raw_event_after(
        &mut stream,
        Duration::ZERO,
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    )
    .expect("record terminal");
    let cassette = ProviderStreamCassette {
        version: PROVIDER_STREAM_CASSETTE_VERSION,
        request: standard_body.clone(),
        stream,
    };

    validate_provider_stream_cassette_candidates(
        "mode-test",
        &cassette,
        std::slice::from_ref(&standard_body),
    )
    .expect("matching mode");
    assert!(matches!(
        validate_provider_stream_cassette_candidates(
            "mode-test",
            &cassette,
            std::slice::from_ref(&lite_body),
        ),
        Err(LlmError::Vcr(tau_vcr::VcrError::RequestMismatch { .. }))
    ));
}

/// Provider cassette validation must enforce every resource bound before
/// replay so malformed evidence cannot consume unbounded memory or time.
#[test]
fn provider_cassette_validation_rejects_resource_limit_violations() {
    let request = serde_json::json!({"model": "synthetic"});
    let cases = [
        ProviderRawEventStream::default(),
        ProviderRawEventStream {
            raw_events: vec![ProviderRawEvent {
                delta_micros: MAX_PROVIDER_CASSETTE_DELTA_MICROS + 1,
                raw: "{}".to_owned(),
            }],
        },
        ProviderRawEventStream {
            raw_events: vec![ProviderRawEvent {
                delta_micros: 0,
                raw: "x".repeat(MAX_PROVIDER_CASSETTE_FRAME_BYTES + 1),
            }],
        },
        ProviderRawEventStream {
            raw_events: vec![
                ProviderRawEvent {
                    delta_micros: 0,
                    raw: "{}".to_owned(),
                };
                MAX_PROVIDER_CASSETTE_EVENTS + 1
            ],
        },
        ProviderRawEventStream {
            raw_events: vec![
                ProviderRawEvent {
                    delta_micros: 0,
                    raw: "x".repeat(MAX_PROVIDER_CASSETTE_RAW_BYTES / 4 + 1),
                };
                4
            ],
        },
    ];
    for stream in cases {
        let cassette = ProviderStreamCassette {
            version: PROVIDER_STREAM_CASSETTE_VERSION,
            request: request.clone(),
            stream,
        };
        assert!(matches!(
            validate_provider_stream_cassette_candidates(
                "limits",
                &cassette,
                std::slice::from_ref(&request),
            ),
            Err(LlmError::Vcr(tau_vcr::VcrError::InvalidCassette { .. }))
        ));
    }
}

/// Online recording enforces the same bounds as replay validation so a live
/// capture cannot publish evidence that this version later rejects.
#[test]
fn provider_cassette_recording_rejects_resource_limit_violations() {
    let cases = [
        (
            ProviderRawEventStream::default(),
            Duration::ZERO,
            "x".repeat(MAX_PROVIDER_CASSETTE_FRAME_BYTES + 1),
        ),
        (
            ProviderRawEventStream::default(),
            Duration::from_micros(MAX_PROVIDER_CASSETTE_DELTA_MICROS + 1),
            "{}".to_owned(),
        ),
        (
            ProviderRawEventStream {
                raw_events: vec![
                    ProviderRawEvent {
                        delta_micros: 0,
                        raw: "{}".to_owned(),
                    };
                    MAX_PROVIDER_CASSETTE_EVENTS
                ],
            },
            Duration::ZERO,
            "{}".to_owned(),
        ),
        (
            ProviderRawEventStream {
                raw_events: vec![
                    ProviderRawEvent {
                        delta_micros: 0,
                        raw: "x".repeat(MAX_PROVIDER_CASSETTE_RAW_BYTES / 4),
                    };
                    4
                ],
            },
            Duration::ZERO,
            "x".to_owned(),
        ),
    ];
    for (mut stream, delta, raw) in cases {
        let initial = stream.clone();
        let error = record_provider_raw_event_after(&mut stream, delta, raw)
            .expect_err("recording limit violation");
        assert!(matches!(
            error,
            LlmError::Vcr(tau_vcr::VcrError::InvalidCassette { .. })
        ));
        assert_eq!(stream, initial);
    }
}

/// Unknown persisted fields fail closed before public-fixture allowlisting so
/// serde cannot silently discard an unreviewed sensitive side channel.
#[test]
fn provider_cassette_schema_rejects_unknown_fields() {
    let yaml = r#"
version: 0
unexpected_private_value: short-secret
request: {}
stream:
  raw_events: []
"#;
    assert!(serde_yaml_ng::from_str::<ProviderStreamCassette>(yaml).is_err());
}
/// WebSocket replay has the same terminal requirement as its live transport;
/// exhausting frames after partial output must remain a transport failure.
#[test]
fn provider_websocket_replay_rejects_stream_without_terminal_event() {
    let mut stream = ProviderRawEventStream::default();
    record_provider_raw_event_after(
        &mut stream,
        Duration::ZERO,
        "{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}",
    )
    .expect("record test event");

    let error = match ws::run_replay(&stream, ws::ResponseMode::Ordinary, &mut |_| {}) {
        Ok(_) => panic!("truncated replay must not succeed"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("provider stream ended without terminal event")
    );
}
/// Ensures the WebSocket request carries the Responses Lite routing marker as
/// per-request metadata, allowing pooled sockets to serve different modes.
#[test]
fn ws_envelope_carries_responses_lite_request_metadata() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-luna".into(),
        ..chain_test_config()
    };
    let request = basic_prompt_payload();

    let body =
        serde_json::to_value(build_ws_envelope(&config, &request, None, None)).expect("serialize");

    assert_eq!(
        body["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
        "true"
    );
}

/// Ensures WebSocket GPT-5.6 requests retain the Lite routing marker while
/// suppressing incompatible server-side compaction controls.
#[test]
fn ws_envelope_suppresses_compaction_without_disabling_responses_lite() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::LiteCompatibility,
        model_id: "gpt-5.6-luna".into(),
        ..chain_test_config()
    };
    let mut request = basic_prompt_payload();
    request.compaction = Some(tau_proto::PromptCompactionContext {
        compact_threshold: Some(tau_proto::TokenCount::new(1200)),
    });

    let body =
        serde_json::to_value(build_ws_envelope(&config, &request, None, None)).expect("serialize");

    assert_eq!(
        body["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
        "true"
    );
    assert!(body.get("context_management").is_none());
}

/// Ensures a standard route with inline-compaction support retains context
/// management and explicit trigger items.
#[test]
fn build_request_sends_compaction_context_management_and_trigger_item() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_compaction: true,
        ..chain_test_config()
    };
    let items = [ContextItem::CompactionTrigger];
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: Some(tau_proto::TokenCount::new(1200)),
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");

    assert_eq!(body["context_management"][0]["type"], "compaction");
    assert_eq!(body["context_management"][0]["compact_threshold"], 1200);
    assert_eq!(body["input"][0]["type"], "compaction_trigger");
}

/// GPT-5.6 v2 standalone compaction sends the complete window and final trigger
/// on a fresh ordinary Responses chain.
#[test]
fn build_request_sends_v2_compaction_as_fresh_full_window() {
    for mode in [ResponsesMode::Standard, ResponsesMode::LiteCompatibility] {
        let config = ResponsesConfig {
            mode,
            supports_compaction: false,
            ..chain_test_config()
        };
        let items = [
            user_text("first"),
            assistant_text("answer"),
            ContextItem::CompactionTrigger,
        ];
        let request = request_for_items(&items);

        let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
        let input = body["input"].as_array().expect("input");

        assert_eq!(
            input.last().expect("final input")["type"],
            "compaction_trigger"
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "compaction_trigger")
                .count(),
            1
        );
        assert!(body.get("previous_response_id").is_none());
    }
}

#[test]
fn build_request_trims_full_replay_before_latest_compaction_item() {
    let config = ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_compaction: true,
        ..chain_test_config()
    };
    let compaction_item = serde_json::json!({
        "type": "compaction",
        "summary": "old history",
    });
    let items = [
        user_text("obsolete"),
        ContextItem::Compaction(
            OpaqueProviderItem::from_raw_json(compaction_item.to_string())
                .expect("valid compaction item"),
        ),
        user_text("new"),
    ];
    let request = PromptPayload {
        system_prompt: "system",
        context: context(&items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: Some(tau_proto::PromptCompactionContext {
            compact_threshold: None,
        }),
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input array");

    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "compaction");
    assert_eq!(input[1]["content"][0]["text"], "new");
    assert_eq!(body["context_management"][0]["compact_threshold"], 232560);
}

fn chain_test_config() -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".into(),
        api_key: "test".into(),
        model_id: "gpt-5-codex".into(),
        raw_context_window: tau_proto::TokenCount::new(258400),
        account_id: None,
        supports_reasoning_effort: false,
        supports_verbosity: false,
        supports_phase: false,
        supports_reasoning_summary: false,
        supports_compaction: false,
        supports_prompt_cache_key: false,
        supports_encrypted_reasoning: false,
    }
}

fn phase_test_config() -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_phase: true,
        ..chain_test_config()
    }
}

fn encrypted_reasoning_test_config() -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: ResponsesMode::Standard,
        supports_encrypted_reasoning: true,
        ..chain_test_config()
    }
}

fn basic_prompt_payload() -> PromptPayload<'static> {
    let session_id = Box::leak(Box::new(
        tau_proto::SessionId::parse("test-session").expect("known-safe SessionId must be valid"),
    ));
    let agent_id = Box::leak(Box::new(
        tau_proto::AgentId::parse("test-agent").expect("agent id"),
    ));
    PromptPayload {
        system_prompt: "system",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id,
        agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    }
}

/// Standard Responses must lower cached hosted search with exact optional
/// domain/context controls while preserving ordinary tool order.
#[test]
fn standard_responses_lowers_hosted_web_search_exactly() {
    let hosted = [tau_proto::HostedToolDefinition::WebSearch {
        access: tau_proto::ProviderWebSearchAccess::Cached,
        context_size: Some(tau_proto::WebSearchContextSize::High),
        allowed_domains: vec!["docs.rs".to_owned(), "rust-lang.org".to_owned()],
    }];
    let mut request = basic_prompt_payload();
    request.hosted_tools = &hosted;
    let body =
        serde_json::to_value(build_request(&chain_test_config(), &request, None)).expect("request");
    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "type": "web_search",
            "external_web_access": false,
            "search_context_size": "high",
            "filters": {"allowed_domains": ["docs.rs", "rust-lang.org"]}
        }])
    );
}

/// Citation indices are converted over Unicode scalars, unsafe annotations are
/// discarded, and exact raw provider syntax remains the replay sidecar.
#[test]
fn response_message_retains_bounded_unicode_url_citations() {
    let mut state = path_crate_common::StreamState::new();
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "α source",
                    "annotations": [
                        {"type":"url_citation","start_index":2,"end_index":8,
                         "url":"https://example.com/a","title":"Example"},
                        {"type":"url_citation","start_index":0,"end_index":1,
                         "url":"javascript:alert(1)","title":"unsafe"}
                    ]
                }]
            }
        }),
        &mut |_| {},
    )
    .expect("message done");
    let items = state.into_output_items();
    let ContextItem::Message(message) = &items[0] else {
        panic!("message")
    };
    assert!(matches!(
        &message.content[1],
        ContentPart::UrlCitation { citation }
            if citation.start() == 2
                && citation.end() == 8
                && citation.url() == "https://example.com/a"
                && citation.title() == "Example"
    ));
    assert_eq!(message.content.len(), 3);
    assert!(matches!(
        message.content[2],
        ContentPart::CitationMetadataInvalid
    ));
    assert!(message.responses_raw_json.is_some());
}

/// Canonical percent-encoding expansion cannot create a semantic citation that
/// the shared OSC 8 target bound would later render as inert text.
#[test]
fn oversized_canonical_citation_url_becomes_invalid_metadata() {
    let mut state = path_crate_common::StreamState::new();
    let expanded = format!("https://example.com/{}", "😀".repeat(500));
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "answer",
                    "annotations": [{
                        "type":"url_citation", "start_index":0, "end_index":6,
                        "url":expanded, "title":"expanded"
                    }]
                }]
            }
        }),
        &mut |_| {},
    )
    .expect("answer survives invalid citation");
    let ContextItem::Message(message) = &state.into_output_items()[0] else {
        panic!("message")
    };
    assert!(matches!(
        message.content.as_slice(),
        [ContentPart::Text { text }, ContentPart::CitationMetadataInvalid]
            if text == "answer"
    ));
}

/// Hosted web-search lifecycle drives transient activity while its completed
/// item remains opaque provider replay data.
#[test]
fn hosted_web_search_activity_is_transient_and_completion_stays_opaque() {
    let mut state = StreamState::new();
    apply_parsed_json_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type":"web_search_call","id":"ws_1","status":"in_progress"}
        }),
        None,
        &mut |_| {},
    )
    .expect("hosted search added");
    assert!(state.web_search_active());

    let raw = r#"{"type":"web_search_call","id":"ws_1","status":"completed"}"#;
    apply_parsed_json_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type":"web_search_call","id":"ws_1","status":"completed"}
        }),
        Some(raw),
        &mut |_| {},
    )
    .expect("hosted search done");
    assert!(!state.web_search_active());
    let items = state.into_output_items();
    let ContextItem::UnknownProviderItem(item) = &items[0] else {
        panic!("opaque hosted completion")
    };
    assert_eq!(item.raw_json(), raw);
}

/// Ensures private Responses appends the newest context after its existing
/// provider-visible context while retaining stable declarations.
#[test]
fn responses_request_appends_context_after_existing_provider_input() {
    let config = chain_test_config();
    let tools = [tau_proto::ToolDefinition {
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
    }];
    let stable_context = context(&[user_text("first stable turn")]);
    let stable_payload = PromptPayload {
        system_prompt: "stable system authority",
        context: stable_context,
        hosted_tools: &[],
        tools: &tools,
        ..basic_prompt_payload()
    };
    let stable = serde_json::to_value(build_request(&config, &stable_payload, None))
        .expect("serialize stable request");

    let next_context = context(&[
        user_text("first stable turn"),
        user_text("newest volatile turn"),
    ]);
    let next_payload = PromptPayload {
        context: next_context,
        ..stable_payload
    };
    let next = serde_json::to_value(build_request(&config, &next_payload, None))
        .expect("serialize next request");
    let stable_input = stable["input"].as_array().expect("stable input");
    let next_input = next["input"].as_array().expect("next input");
    assert_eq!(&next_input[..stable_input.len()], stable_input);
    assert_eq!(next["instructions"], stable["instructions"]);
    assert_eq!(next["tools"], stable["tools"]);
}

struct TestAbortWaker;

impl crate::TurnAbortWaker for TestAbortWaker {}

/// Standalone compaction must recheck cancellation after waker registration so
/// a conforming source need not retroactively wake a late registration.
#[test]
fn compact_abort_rechecks_cancellation_after_waker_registration() {
    struct AbortDuringRegistration(bool);
    impl crate::TurnAbort for AbortDuringRegistration {
        fn is_aborted(&mut self) -> bool {
            self.0
        }

        fn register_waker(
            &mut self,
            _waker: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
        ) -> Box<dyn crate::TurnAbortWaker> {
            self.0 = true;
            Box::new(TestAbortWaker)
        }
    }

    let completion = path_std_sync::Arc::new((
        path_std_sync::Mutex::new(CompactCompletion::default()),
        path_std_sync::Condvar::new(),
    ));
    let mut abort = AbortDuringRegistration(false);
    assert!(matches!(
        register_compact_abort_waker(
            &mut abort,
            &completion,
            &path_std_sync::Arc::new(tokio::sync::Notify::new()),
        ),
        Err(LlmError::Canceled)
    ));
}
fn user_text(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn assistant_text(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn assistant_text_with_phase(text: &str, phase: tau_proto::MessagePhase) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: Some(phase),
        responses_raw_json: None,
    })
}

fn assistant_text_with_phase_and_raw(
    text: &str,
    phase: tau_proto::MessagePhase,
    raw_json: &str,
) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: Some(phase),
        responses_raw_json: Some(raw_json.to_owned()),
    })
}

fn assistant_tool_call(
    id: &str,
    name: &str,
    tool_type: tau_proto::ToolType,
    input: tau_proto::CborValue,
) -> ContextItem {
    ContextItem::ToolCall(ToolCallItem {
        call_id: id.into(),
        name: tau_proto::ToolName::new(name),
        tool_type,
        arguments: input,
        raw_arguments_json: None,
        responses_envelope: None,
    })
}

fn restored_internal_tool_error(call_id: &str, body: &str) -> ContextItem {
    ContextItem::ToolResult(ToolResultItem {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Error {
            message: format!(
                "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
                tau_proto::TAU_INTERNAL_HEADER_NAME
            ),
        },
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(body.to_owned())),
        provider_content: Vec::new(),
    })
}

fn reasoning_item(item: &str) -> ContextItem {
    ContextItem::Reasoning(
        OpaqueProviderItem::from_raw_json(item).expect("valid reasoning item json"),
    )
}

fn raw_reasoning_item(item: &str) -> ContextItem {
    ContextItem::Reasoning(
        OpaqueProviderItem::from_raw_json(item).expect("valid reasoning item json"),
    )
}

/// Completed opaque output cannot enter retained state without its exact raw
/// provider item JSON.
#[test]
fn completed_opaque_item_without_raw_json_is_rejected_before_mutation() {
    let event = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {"type": "compaction", "encrypted_content": "opaque"}
    });
    let mut state = StreamState::new();

    assert!(matches!(
        apply_parsed_json_event(&mut state, &event, None, &mut |_| {}),
        Err(LlmError::InvalidResponse(_))
    ));
    assert!(state.output_items.is_empty());
}

fn request_for_items(items: &[ContextItem]) -> PromptPayload<'static> {
    let session_id = Box::leak(Box::new(
        tau_proto::SessionId::parse("test-session").expect("known-safe SessionId must be valid"),
    ));
    let agent_id = Box::leak(Box::new(
        tau_proto::AgentId::parse("test-agent").expect("agent id"),
    ));
    PromptPayload {
        system_prompt: "sys",
        context: context(items),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id,
        agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    }
}

/// When `supports_phase` is on, every assistant `message` item must
/// carry a `phase` field. A stored `Commentary` value rides straight
/// through; absence of a stored value falls back to `final_answer`
/// per the OpenAI deployment-checklist guidance for legacy history.
#[test]
fn build_request_stamps_phase_on_assistant_messages_when_supported() {
    let config = phase_test_config();
    let messages = vec![
        user_text("hello"),
        assistant_text_with_phase("draft", tau_proto::MessagePhase::Commentary),
        user_text("more"),
        assistant_text("legacy turn without phase"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");

    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(assistant_items.len(), 2, "two assistant messages expected");
    assert_eq!(assistant_items[0]["phase"], "commentary");
    assert_eq!(
        assistant_items[1]["phase"], "final_answer",
        "legacy assistant message must default to final_answer per OpenAI guidance"
    );
}

/// `supports_phase: false` keeps the field off the wire entirely,
/// even when the stored message carries one. This is the safety
/// gate that lets older Codex models (which would reject unknown
/// fields) keep working as the harness sends them history that may
/// have been captured against a newer model.
#[test]
fn build_request_omits_phase_when_unsupported() {
    let config = chain_test_config(); // supports_phase: false
    let messages = vec![assistant_text_with_phase(
        "draft",
        tau_proto::MessagePhase::Commentary,
    )];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_item = input
        .iter()
        .find(|i| i["role"].as_str() == Some("assistant"))
        .expect("assistant message");
    assert!(
        assistant_item
            .as_object()
            .expect("assistant item is an object")
            .get("phase")
            .is_none(),
        "phase must not be sent when the backend doesn't advertise support"
    );
}

/// Tool-call boundaries flush a pending text block into its own
/// assistant `message` item. That intermediate flush must carry
/// `phase` too — otherwise a mixed text+tool_use turn would
/// half-pass-through with a phase on the trailing flush only.
#[test]
fn build_request_stamps_phase_on_pre_tool_call_text_flush() {
    let config = phase_test_config();
    let messages = vec![
        assistant_text_with_phase("thinking out loud", tau_proto::MessagePhase::Commentary),
        assistant_tool_call(
            "call-1",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Null,
        ),
        assistant_text_with_phase("trailing", tau_proto::MessagePhase::Commentary),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let assistant_items: Vec<&serde_json::Value> = input
        .iter()
        .filter(|item| item["role"].as_str() == Some("assistant"))
        .collect();
    assert_eq!(
        assistant_items.len(),
        2,
        "pre-tool-call text and trailing text each become their own assistant message"
    );
    for (i, item) in assistant_items.iter().enumerate() {
        assert_eq!(
            item["phase"], "commentary",
            "assistant message #{i} must carry the captured phase"
        );
    }
}

/// `parse_phase_from_item` is the bridge from the Responses-API
/// `output_item.*` shape into our typed `MessagePhase`. The two
/// known wire strings round-trip; unknown values (forward
/// compatibility) come back as `None` rather than panicking.
#[test]
fn parse_phase_from_item_recognizes_wire_strings() {
    let commentary = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&commentary),
        Some(tau_proto::MessagePhase::Commentary)
    );

    let final_ans = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "final_answer",
    });
    assert_eq!(
        parse_phase_from_item(&final_ans),
        Some(tau_proto::MessagePhase::FinalAnswer)
    );

    let unknown_future = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "phase": "rumination",
    });
    assert_eq!(parse_phase_from_item(&unknown_future), None);

    let no_phase = serde_json::json!({
        "type": "message",
        "role": "assistant",
    });
    assert_eq!(parse_phase_from_item(&no_phase), None);

    let function_call = serde_json::json!({
        "type": "function_call",
        "phase": "commentary",
    });
    assert_eq!(
        parse_phase_from_item(&function_call),
        None,
        "non-message items must not have their `phase` field harvested"
    );
}

// -----------------------------------------------------------------------
// Encrypted reasoning replay
// -----------------------------------------------------------------------

/// `supports_encrypted_reasoning: true` must put
/// `include: ["reasoning.encrypted_content"]` on the request body.
/// Without this opt-in the model returns `reasoning` items but with
/// no replayable content — we'd persist empty husks and lose the
/// continuity the whole feature buys.
#[test]
fn build_request_emits_include_when_encrypted_reasoning_supported() {
    let config = encrypted_reasoning_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let include = body["include"].as_array().expect("include array");
    assert_eq!(include.len(), 1);
    assert_eq!(include[0], "reasoning.encrypted_content");
}

/// `supports_encrypted_reasoning: false` keeps the `include` field
/// out of the request entirely — older endpoints (and the public
/// Responses API) reject unknown opt-ins, so we don't even want an
/// empty `include: []` on the wire.
#[test]
fn build_request_omits_include_when_encrypted_reasoning_unsupported() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    assert!(
        body.as_object()
            .expect("request body is an object")
            .get("include")
            .is_none(),
        "include must be omitted when the provider doesn't advertise support"
    );
}

/// A `ContentBlock::Reasoning` on an assistant message must be
/// emitted as its own top-level `input[]` item — same structural
/// slot as `message` and `function_call`, NEVER nested inside the
/// assistant message. Locks in the Pi-compatible replay shape; if
/// this regresses, the model loses reasoning continuity across a
/// broken chain.
#[test]
fn build_request_replays_reasoning_item_as_top_level_input() {
    let config = encrypted_reasoning_test_config();
    let reasoning_blob = serde_json::json!({
        "type": "reasoning",
        "id": "rs_abc123",
        "summary": [],
        "encrypted_content": "OPAQUE-BLOB"
    })
    .to_string();
    let messages = vec![
        reasoning_item(&reasoning_blob),
        assistant_text("here's the answer"),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };
    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let reasoning_idx = input
        .iter()
        .position(|item| item["type"].as_str() == Some("reasoning"))
        .expect("reasoning input item");
    let message_idx = input
        .iter()
        .position(|item| item["role"].as_str() == Some("assistant"))
        .expect("assistant message item");
    assert!(
        reasoning_idx < message_idx,
        "reasoning must precede the assistant message it relates to (Pi-compatible order); \
         reasoning_idx={reasoning_idx}, message_idx={message_idx}"
    );
    let reasoning = &input[reasoning_idx];
    assert_eq!(reasoning["id"], "rs_abc123");
    assert_eq!(
        reasoning["encrypted_content"], "OPAQUE-BLOB",
        "the opaque blob must round-trip verbatim — the harness must not parse fields out"
    );
}

/// Opaque Responses reasoning items carry provider-owned JSON whose lexical
/// shape can matter to upstream cache identity. Replay must therefore prefer
/// the raw sidecar instead of serializing the parsed CBOR value, preserving key
/// order and numeric spelling for unknown provider-visible fields.
#[test]
fn build_request_replays_reasoning_item_from_raw_json_sidecar() {
    let config = encrypted_reasoning_test_config();
    let raw_reasoning = r#"{"type":"reasoning","z":1.2300,"a":1e+03,"id":"rs_raw","encrypted_content":"SEALED","summary":[]}"#;
    let messages = vec![raw_reasoning_item(raw_reasoning), assistant_text("answer")];
    let request = request_for_items(&messages);

    let body = serde_json::to_string(&build_request(&config, &request, None)).expect("serialize");

    assert!(
        body.contains(raw_reasoning),
        "raw reasoning JSON sidecar must be embedded verbatim; body={body}"
    );
}

/// Assistant Responses `message` items carry provider-owned envelope and
/// content-part metadata that Tau does not otherwise model. When the typed text
/// and phase still match, full replay should embed the raw sidecar unchanged so
/// ids, status, annotations, part ids, and unknown fields keep their original
/// provider-visible shape.
#[test]
fn build_request_replays_matching_assistant_message_from_raw_sidecar() {
    let config = phase_test_config();
    let raw_message = r#"{"type":"message","id":"msg_raw","status":"completed","role":"assistant","phase":"commentary","content":[{"type":"output_text","id":"part_a","text":"hello","annotations":[{"type":"url_citation","url":"https://example.test"}],"future_part":true}],"future_message":123}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_string(&build_request(&config, &request, None)).expect("serialize");

    assert!(
        body.contains(raw_message),
        "matching raw assistant message sidecar must be embedded verbatim; body={body}"
    );
}

/// A captured Responses message can contain multiple text content parts with
/// independent provider ids/annotations. Tau's typed assistant text is a
/// semantic projection, so unchanged text must still replay through the raw
/// sidecar instead of collapsing those provider-visible part boundaries.
#[test]
fn build_request_keeps_raw_assistant_message_part_boundaries() {
    let config = phase_test_config();
    let raw_message = r#"{"type":"message","id":"msg_raw","role":"assistant","phase":"commentary","content":[{"type":"output_text","id":"part_a","text":"he","annotations":[]},{"type":"output_text","id":"part_b","text":"llo","annotations":[{"type":"file_citation","file_id":"file_1"}]}]}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_string(&build_request(&config, &request, None)).expect("serialize");

    assert!(
        body.contains(raw_message),
        "raw assistant message content-part boundaries must be embedded verbatim; body={body}"
    );
}

/// Raw assistant-message sidecars are provider syntax, not semantic authority.
/// A sidecar whose text and phase match typed Tau fields but whose role/type is
/// not an assistant Responses message must be ignored rather than replayed
/// verbatim.
#[test]
fn build_request_rejects_raw_assistant_message_with_wrong_role() {
    let config = phase_test_config();
    let raw_message = r#"{"type":"message","id":"msg_bad","role":"user","phase":"commentary","content":[{"type":"output_text","text":"hello","annotations":[]}]}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_string(&build_request(&config, &request, None)).expect("serialize");
    assert!(
        !body.contains(r#""role":"user""#),
        "invalid raw sidecar must not override typed assistant role; body={body}"
    );
    assert!(
        body.contains(r#""role":"assistant""#),
        "typed assistant message should be synthesized instead; body={body}"
    );
}

/// Matching text and phase are not enough to trust a raw sidecar; the item must
/// also be a Responses `message`, not another provider item shape.
#[test]
fn build_request_rejects_raw_assistant_message_with_wrong_type() {
    let config = phase_test_config();
    let raw_message = r#"{"type":"future_item","id":"msg_bad","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"hello","annotations":[]}]}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_string(&build_request(&config, &request, None)).expect("serialize");
    assert!(
        !body.contains(r#""type":"future_item""#),
        "invalid raw sidecar must not override typed assistant item type; body={body}"
    );
    assert!(
        body.contains(r#""type":"message""#),
        "typed assistant message should be synthesized instead; body={body}"
    );
}

/// When replaying to a model that does not support assistant-message `phase`,
/// the raw sidecar still preserves provider-owned metadata, but the outgoing
/// item must strip `phase` to satisfy that model's wire contract.
#[test]
fn build_request_strips_phase_from_raw_assistant_message_when_unsupported() {
    let config = chain_test_config();
    let raw_message = r#"{"type":"message","id":"msg_raw","status":"completed","role":"assistant","phase":"commentary","content":[{"type":"output_text","id":"part_a","text":"hello","annotations":[]}],"future_message":123}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let message = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"].as_str() == Some("message"))
        .expect("assistant message");

    assert_eq!(message["id"], "msg_raw");
    assert_eq!(message["status"], "completed");
    assert_eq!(message["future_message"], 123);
    assert!(message.get("phase").is_none(), "phase must be stripped");
}

/// Non-string raw `phase` values still violate models that do not support the
/// field. They must force the rebase path so the key is removed instead of
/// passing the raw item through unchanged.
#[test]
fn build_request_strips_non_string_phase_from_raw_assistant_message_when_unsupported() {
    let config = chain_test_config();
    let raw_message = r#"{"type":"message","id":"msg_raw","role":"assistant","phase":{"future":true},"content":[{"type":"output_text","id":"part_a","text":"hello","annotations":[]}]}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "hello",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let message = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"].as_str() == Some("message"))
        .expect("assistant message");

    assert_eq!(message["id"], "msg_raw");
    assert_eq!(message["content"][0]["id"], "part_a");
    assert!(message.get("phase").is_none(), "phase must be stripped");
}

/// The raw Responses message sidecar is not semantic authority. If typed Tau
/// text/phase differ from the captured provider item, replay must keep the
/// provider-owned envelope where possible but update only the model-visible
/// text and phase from the typed fields.
#[test]
fn build_request_rebases_raw_assistant_message_text_and_phase() {
    let config = phase_test_config();
    let raw_message = r#"{"type":"message","id":"msg_raw","status":"completed","role":"assistant","phase":"final_answer","content":[{"type":"output_text","id":"part_a","text":"old","annotations":[{"type":"url_citation","url":"https://example.test"}],"future_part":true}],"future_message":123}"#;
    let messages = vec![assistant_text_with_phase_and_raw(
        "new",
        tau_proto::MessagePhase::Commentary,
        raw_message,
    )];
    let request = request_for_items(&messages);

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    let message = input
        .iter()
        .find(|item| item["type"].as_str() == Some("message"))
        .expect("assistant message");

    assert_eq!(message["id"], "msg_raw");
    assert_eq!(message["status"], "completed");
    assert_eq!(message["future_message"], 123);
    assert_eq!(message["phase"], "commentary");
    assert_eq!(message["content"][0]["id"], "part_a");
    assert_eq!(message["content"][0]["future_part"], true);
    assert_eq!(
        message["content"][0]["annotations"][0]["url"],
        "https://example.test"
    );
    assert_eq!(message["content"][0]["text"], "new");
}

#[test]
fn build_request_emits_custom_tool_definition_and_round_trips_custom_tool_output() {
    let config = chain_test_config();
    let tool = tau_proto::ToolDefinition {
        name: tau_proto::ToolName::new("apply_patch"),
        model_visible_name: None,
        description: Some("Apply a patch to files".to_owned()),
        tool_type: tau_proto::ToolType::Custom,
        parameters: None,
        format: Some(tau_proto::ToolFormat::Grammar {
            syntax: tau_proto::ToolGrammarSyntax::Regex,
            definition: "(?s).+".to_owned(),
        }),
    };
    let messages = vec![
        assistant_tool_call(
            "call-patch",
            "apply_patch",
            tau_proto::ToolType::Custom,
            tau_proto::CborValue::Text("*** Begin Patch\n*** End Patch".into()),
        ),
        ContextItem::ToolResult(ToolResultItem {
            presentation: Default::default(),
            call_id: "call-patch".into(),
            tool_type: tau_proto::ToolType::Custom,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("ok".into())),
            provider_content: Vec::new(),
        }),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: std::slice::from_ref(&tool),
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["type"], "custom");
    assert_eq!(tools[0]["name"], "apply_patch");
    assert_eq!(tools[0]["format"]["type"], "grammar");
    assert_eq!(tools[0]["format"]["syntax"], "regex");
    assert_eq!(tools[0]["format"]["definition"], "(?s).+");

    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "custom_tool_call");
    assert_eq!(input[0]["id"], "ctc_call-patch");
    assert_eq!(input[0]["call_id"], "call-patch");
    assert_eq!(input[0]["input"], "*** Begin Patch\n*** End Patch");
    assert_eq!(input[1]["type"], "custom_tool_call_output");
    assert_eq!(input[1]["call_id"], "call-patch");
    assert_eq!(input[1]["output"], "ok");
}

/// Provider item ids are separate from Tau call ids and must be prefixed for
/// the Responses API, but replay can contain ids already captured from that
/// API. Pin both cases so request conversion does not double-prefix history
/// or accidentally send unprefixed local ids.
#[test]
fn build_request_preserves_existing_provider_tool_call_id_prefixes() {
    let config = chain_test_config();
    let messages = vec![
        assistant_tool_call(
            "fc_existing",
            "shell",
            tau_proto::ToolType::Function,
            tau_proto::CborValue::Null,
        ),
        assistant_tool_call(
            "ctc_existing",
            "apply_patch",
            tau_proto::ToolType::Custom,
            tau_proto::CborValue::Text("patch".into()),
        ),
    ];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["id"], "fc_existing");
    assert_eq!(input[0]["call_id"], "fc_existing");
    assert_ne!(input[0]["id"], "fc_fc_existing");
    assert_eq!(input[1]["type"], "custom_tool_call");
    assert_eq!(input[1]["id"], "ctc_existing");
    assert_eq!(input[1]["call_id"], "ctc_existing");
    assert_ne!(input[1]["id"], "ctc_ctc_existing");
}

/// Cancelled tool results are replayed as provider output items rather than
/// dropped. The rendered cancellation header is the only durable explanation
/// the model sees on replay, so keep its wire type, call id, and text shape
/// pinned across conversion refactors.
#[test]
fn build_request_replays_cancelled_tool_result_with_header() {
    let config = chain_test_config();
    let messages = vec![ContextItem::ToolResult(ToolResultItem {
        presentation: Default::default(),
        call_id: "call-cancelled".into(),
        tool_type: tau_proto::ToolType::Function,
        status: ToolResultStatus::Cancelled {
            reason: "user interrupted".to_owned(),
        },
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
        provider_content: Vec::new(),
    })];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_request(&config, &request, None)).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "function_call_output");
    assert_eq!(input[0]["call_id"], "call-cancelled");
    assert_eq!(input[0]["output"], "cancelled: user interrupted\n\n");
}

#[test]
fn apply_event_accumulates_custom_tool_input_deltas() {
    use crate::common::StreamState;

    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
        }
    });
    apply_event(&mut state, &added, &mut |_| {}).expect("added");
    let delta = serde_json::json!({
        "type": "response.custom_tool_call_input.delta",
        "output_index": 0,
        "delta": "*** Begin Patch"
    });
    apply_event(&mut state, &delta, &mut |_| {}).expect("delta");
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "call_id": "call_patch",
            "name": "apply_patch",
            "input": "*** Begin Patch"
        }
    });
    apply_event(&mut state, &done, &mut |_| {}).expect("done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected custom tool call item");
    };
    assert_eq!(call.tool_type, tau_proto::ToolType::Custom);
    assert_eq!(call.call_id.as_str(), "call_patch");
    assert_eq!(call.name.as_str(), "apply_patch");
    assert_eq!(call.raw_arguments_json, None);
    assert_eq!(
        call.arguments,
        tau_proto::CborValue::Text("*** Begin Patch".into())
    );
}

/// A compatible tool-calling response anchor must keep its function result as
/// the only incremental input rather than forcing a full transcript replay.
#[test]
fn build_request_chain_keeps_custom_tool_output_type_from_prior_history() {
    let config = chain_test_config();
    let tool_result = ToolResultItem {
        presentation: Default::default(),
        call_id: "call-custom".into(),
        tool_type: tau_proto::ToolType::Custom,
        status: ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("ok".into())),
        provider_content: Vec::new(),
    };
    let request = PromptPayload {
        system_prompt: "sys",
        context: Box::leak(Box::new(tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                    provider_response_id: Some("resp_custom".to_owned()),
                    backend: None,
                    output_items: vec![assistant_tool_call(
                        "call-custom",
                        "apply_patch",
                        tau_proto::ToolType::Custom,
                        tau_proto::CborValue::Text("patch body".into()),
                    )],
                    usage: None,
                }),
                tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                    items: vec![tool_result],
                }),
            ],
        })),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let anchor =
        response_anchor_from_context(request.context, "resp_custom").expect("response anchor");
    let body =
        serde_json::to_value(build_request(&config, &request, Some(&anchor))).expect("serialize");
    let input = body["input"].as_array().expect("input");
    assert_eq!(
        input.len(),
        1,
        "only the trailing tool result should be replayed"
    );
    assert_eq!(input[0]["type"], "custom_tool_call_output");
    assert_eq!(input[0]["call_id"], "call-custom");
}

/// On the Codex Responses stream, `response.output_item.done` is the
/// canonical place to capture a `reasoning` item: it's the only
/// event that carries the final `encrypted_content`. The `added`
/// counterpart fires before any content streams in, so capturing
/// from `added` would persist empty husks. Pin the boundary here so
/// a future refactor of the SSE/WS parser can't silently swap which
/// event we read.
#[test]
fn apply_event_captures_reasoning_only_on_output_item_done() {
    use crate::common::StreamState;
    let mut state = StreamState::new();
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_pending",
            "summary": [],
        }
    });
    apply_event(&mut state, &added, &mut |_| {}).expect("added");
    assert!(
        state
            .output_items
            .iter()
            .all(|item| matches!(item, crate::common::OutputItemAccumulator::Empty)),
        "`added` carries no encrypted_content — capturing here would persist an empty husk"
    );
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "reasoning",
            "id": "rs_done",
            "summary": [{"type": "summary_text", "text": "thought"}],
            "encrypted_content": "SEALED",
        }
    });
    apply_event(&mut state, &done, &mut |_| {}).expect("done");
    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::Reasoning(item) = &items[0] else {
        panic!("expected reasoning item");
    };
    let parsed = crate::common::cbor_to_json(item.value());
    assert_eq!(parsed["id"], "rs_done");
    assert_eq!(parsed["encrypted_content"], "SEALED");
}

/// Reasoning capture on the raw SSE/WS event path must keep the provider item
/// JSON sidecar, not only the parsed CBOR projection, so full replay preserves
/// provider-visible key order and numeric spelling.
#[test]
fn apply_raw_json_event_preserves_reasoning_item_raw_json_for_replay() {
    let mut state = path_crate_common::StreamState::new();
    let raw_reasoning = r#"{"type":"reasoning","z":1.2300,"a":1e+03,"id":"rs_raw","encrypted_content":"SEALED","summary":[]}"#;
    let raw_event = format!(
        r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_reasoning}}}"#
    );

    apply_raw_json_event(&mut state, &raw_event, &mut |_| {}).expect("reasoning done");

    let items = state.into_output_items();
    let tau_proto::ContextItem::Reasoning(item) = &items[0] else {
        panic!("expected reasoning item");
    };
    assert_eq!(item.raw_json(), raw_reasoning);
    let request = request_for_items(&items);
    let body = serde_json::to_string(&build_request(
        &encrypted_reasoning_test_config(),
        &request,
        None,
    ))
    .expect("body");
    assert!(
        body.contains(raw_reasoning),
        "raw reasoning JSON sidecar must be embedded verbatim; body={body}"
    );
}

/// Message capture on the raw SSE/WS event path must keep the full Responses
/// assistant item sidecar, not only the typed text and phase, so replay can
/// preserve provider-owned envelope and content-part metadata.
#[test]
fn apply_raw_json_event_preserves_assistant_message_raw_json_for_replay() {
    let mut state = path_crate_common::StreamState::new();
    let raw_message = r#"{"type":"message","id":"msg_raw","status":"completed","role":"assistant","phase":"commentary","content":[{"type":"output_text","id":"part_a","text":"hello","annotations":[{"type":"url_citation","url":"https://example.test"}]}],"future_message":true}"#;
    let raw_event =
        format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_message}}}"#);

    apply_raw_json_event(&mut state, &raw_event, &mut |_| {}).expect("message done");

    let items = state.into_output_items();
    let tau_proto::ContextItem::Message(message) = &items[0] else {
        panic!("expected message item");
    };
    assert_eq!(message.responses_raw_json.as_deref(), Some(raw_message));
    let request = request_for_items(&items);
    let body =
        serde_json::to_string(&build_request(&phase_test_config(), &request, None)).expect("body");
    assert!(
        body.contains(raw_message),
        "raw assistant message JSON sidecar must be embedded verbatim; body={body}"
    );
}

/// The parser should not persist raw sidecars for non-assistant `message`
/// items. Sidecars must remain subordinate to typed Tau assistant semantics
/// even before they reach request replay.
#[test]
fn apply_raw_json_event_ignores_non_assistant_message_sidecar() {
    let mut state = path_crate_common::StreamState::new();
    let raw_message = r#"{"type":"message","id":"msg_user","role":"user","content":[{"type":"output_text","text":"hello"}]}"#;
    let raw_event =
        format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_message}}}"#);

    apply_raw_json_event(&mut state, &raw_event, &mut |_| {}).expect("message done");

    assert!(
        state.into_output_items().is_empty(),
        "non-assistant message output item should not become assistant transcript"
    );
}

/// Server-side compaction is returned as an ordinary Responses output item.
/// Keep it in the same ordered item accumulator as messages, reasoning, and
/// tool calls so a compaction item is durable transcript output rather than a
/// side channel that can be lost.
#[test]
fn apply_event_captures_compaction_output_item_in_order() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "before",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("message done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "compaction",
                "summary": "old history",
                "input_items": [{
                    "type": "message",
                    "role": "user",
                    "content": "compacted",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("compaction done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    assert!(matches!(items[0], tau_proto::ContextItem::Message(_)));
    let tau_proto::ContextItem::Compaction(item) = &items[1] else {
        panic!("expected compaction item");
    };
    let parsed = crate::common::cbor_to_json(item.value());
    assert_eq!(parsed["type"], "compaction");
    assert_eq!(parsed["summary"], "old history");
}

/// Compaction items are provider-owned Responses items. Capturing them through
/// the raw event path must keep the exact `item` JSON for later full-transcript
/// replay rather than canonicalizing through `serde_json::Value` and CBOR.
#[test]
fn apply_raw_json_event_preserves_compaction_item_raw_json_for_replay() {
    let mut state = path_crate_common::StreamState::new();
    let raw_compaction =
        r#"{"type":"compaction","z":1.2300,"a":1e+03,"summary":"old history","input_items":[]}"#;
    let raw_event = format!(
        r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_compaction}}}"#
    );

    apply_raw_json_event(&mut state, &raw_event, &mut |_| {}).expect("compaction done");

    let items = state.into_output_items();
    let tau_proto::ContextItem::Compaction(item) = &items[0] else {
        panic!("expected compaction item");
    };
    assert_eq!(item.raw_json(), raw_compaction);
    let request = request_for_items(&items);
    let body =
        serde_json::to_string(&build_request(&chain_test_config(), &request, None)).expect("body");
    assert!(
        body.contains(raw_compaction),
        "raw compaction JSON sidecar must be embedded verbatim; body={body}"
    );
}

/// Unknown Responses output items must not disappear from durable history. The
/// parser reserves their provider index on `added`, stores the raw `done` item,
/// and replay emits that provider-owned JSON before later indexed items.
#[test]
fn apply_raw_json_event_captures_unknown_output_item_in_provider_order() {
    let mut state = path_crate_common::StreamState::new();
    let raw_unknown =
        r#"{"type":"future_provider_item","z":1.2300,"a":1e+03,"payload":{"keep":"raw"}}"#;
    let unknown_added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"future_provider_item","id":"pending"}}"#;
    let message_done = r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}}"#;
    let unknown_done =
        format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_unknown}}}"#);

    apply_raw_json_event(&mut state, unknown_added, &mut |_| {}).expect("unknown added");
    apply_raw_json_event(&mut state, message_done, &mut |_| {}).expect("message done");
    apply_raw_json_event(&mut state, &unknown_done, &mut |_| {}).expect("unknown done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    let tau_proto::ContextItem::UnknownProviderItem(item) = &items[0] else {
        panic!("expected unknown provider item first");
    };
    assert_eq!(item.raw_json(), raw_unknown);
    assert!(matches!(items[1], tau_proto::ContextItem::Message(_)));

    let request = request_for_items(&items);
    let body =
        serde_json::to_string(&build_request(&chain_test_config(), &request, None)).expect("body");
    assert!(
        body.contains(raw_unknown),
        "raw unknown provider JSON sidecar must be embedded verbatim; body={body}"
    );
}

// -----------------------------------------------------------------------
// WebSocket envelope wrapping
// -----------------------------------------------------------------------

/// The WS guide requires every client frame to carry `type:
/// "response.create"` at the top level. The HTTP body does not.
/// [`build_ws_envelope`] is the only place we add the tag — pin it
/// here so a future refactor that drops the wrapper struct can't
/// silently regress it.
#[test]
fn ws_envelope_adds_type_and_drops_stream() {
    let config = chain_test_config();
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let http_body =
        serde_json::to_value(build_request(&config, &request, None)).expect("http body");
    let ws_body = serde_json::to_value(build_ws_envelope(&config, &request, None, None))
        .expect("ws envelope");

    assert_eq!(ws_body["type"], "response.create");
    let ws_object = ws_body.as_object().expect("WS envelope object");
    assert!(
        ws_object.get("stream").is_none(),
        "WS frame must omit `stream` — the WS guide says it's not used and the field is transport-implicit"
    );
    // Every other body shape stays identical so the request-build
    // tests already in this file double as WS-envelope coverage.
    assert!(
        ws_object.get("generate").is_none(),
        "normal streaming WS turns must preserve the old wire shape"
    );
    assert_eq!(ws_body["model"], http_body["model"]);
    assert_eq!(ws_body["store"], http_body["store"]);
    assert_eq!(ws_body["input"], http_body["input"]);
}

#[test]
fn ws_prewarm_envelope_sets_generate_false_and_drops_previous_response() {
    let config = chain_test_config();
    let messages = vec![user_text("AGENTS.md context")];
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&messages),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &tau_proto::SessionId::parse("test-session")
            .expect("known-safe SessionId must be valid"),
        agent_id: &tau_proto::AgentId::parse("test-agent").expect("agent id"),
        share_user_cache_key: false,
        debug_provider_requests: false,
    };

    let body = serde_json::to_value(build_ws_envelope(&config, &request, None, Some(false)))
        .expect("prewarm envelope");

    assert_eq!(body["type"], "response.create");
    assert_eq!(body["generate"], false);
    let object = body.as_object().expect("prewarm envelope object");
    assert!(object.get("stream").is_none());
    assert!(object.get("previous_response_id").is_none());
}

// -----------------------------------------------------------------------
// apply_event — shared Responses event applicator
// -----------------------------------------------------------------------

/// `response.output_text.delta` accumulates its indexed message slot and fires
/// `on_update` once per delta.
#[test]
fn apply_event_text_delta_accumulates_and_notifies() {
    let mut state = path_crate_common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |state: &crate::common::StreamState| {
        updates.push(state.aggregate_assistant_text());
    };

    for chunk in ["hel", "lo, ", "world"] {
        let ev = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": chunk,
        });
        let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
        assert!(!done, "text delta should not terminate the stream");
    }
    assert_eq!(state.aggregate_assistant_text(), "hello, world");
    assert_eq!(updates, vec!["hel", "hello, ", "hello, world"]);
}

#[test]
fn stream_delta_emitter_emits_only_new_assistant_and_reasoning_text() {
    // Streaming response updates are append deltas; this prevents large
    // responses from being copied and sent again on every provider chunk while
    // keeping the final output item accumulator complete.
    let mut state = path_crate_common::StreamState::new();
    let mut emitter = path_crate_common::StreamDeltaEmitter::default();

    state.append_message_delta_at(0, "hel");
    state.append_reasoning_summary_delta_at(1, "plan");
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
                kind: tau_proto::ReasoningTextKind::Summary,
                text: "plan".to_owned(),
            },
        ]
    );

    state.append_message_delta_at(0, "lo");
    state.append_reasoning_summary_delta_at(1, " next");
    assert_eq!(
        emitter.deltas(&state),
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "lo".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Summary,
                text: " next".to_owned(),
            },
        ]
    );
    assert!(emitter.deltas(&state).is_empty());
}

#[test]
fn apply_event_preserves_incremental_output_item_order() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "rs_ordered",
                "encrypted_content": "OPAQUE",
            },
        }),
        &mut on_update,
    )
    .expect("reasoning done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
            },
        }),
        &mut on_update,
    )
    .expect("message added");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 1,
            "delta": "checking",
        }),
        &mut on_update,
    )
    .expect("text delta");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 2,
            "item": {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
            },
        }),
        &mut on_update,
    )
    .expect("tool added");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.done",
            "output_index": 2,
            "arguments": "{\"path\":\"Cargo.toml\"}",
        }),
        &mut on_update,
    )
    .expect("tool args done");

    let items = state.into_output_items();
    assert_eq!(items.len(), 3);
    assert!(matches!(items[0], tau_proto::ContextItem::Reasoning(_)));
    let tau_proto::ContextItem::Message(message) = &items[1] else {
        panic!("expected message item");
    };
    assert_eq!(message.phase, Some(tau_proto::MessagePhase::Commentary));
    assert!(matches!(
        &message.content[0],
        tau_proto::ContentPart::Text { text } if text == "checking"
    ));
    let tau_proto::ContextItem::ToolCall(call) = &items[2] else {
        panic!("expected tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_read");
    assert_eq!(call.name.as_str(), "read");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"path\":\"Cargo.toml\"}")
    );
    assert_eq!(
        crate::common::cbor_to_json(&call.arguments),
        serde_json::json!({ "path": "Cargo.toml" })
    );
}

/// Some Responses streams with tool calls commit assistant message
/// text only on `response.output_item.done`, without earlier
/// `response.output_text.delta` events. Persist that message in item
/// order so commentary immediately before a tool call is not dropped.
#[test]
fn apply_event_output_item_done_hydrates_message_text_before_tool_call() {
    let mut state = path_crate_common::StreamState::new();
    let mut updates: Vec<String> = Vec::new();
    let mut on_update = |state: &crate::common::StreamState| {
        if updates.last() != Some(&state.aggregate_assistant_text()) {
            updates.push(state.aggregate_assistant_text());
        }
    };

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{
                    "type": "output_text",
                    "text": "I'll inspect the file first.",
                }],
            },
        }),
        &mut on_update,
    )
    .expect("message done");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
                "arguments": "{\"path\":\"Cargo.toml\"}",
            },
        }),
        &mut on_update,
    )
    .expect("tool done");

    assert_eq!(updates, vec!["I'll inspect the file first."]);
    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    let tau_proto::ContextItem::Message(message) = &items[0] else {
        panic!("expected message item before tool call");
    };
    assert_eq!(message.phase, Some(tau_proto::MessagePhase::Commentary));
    assert!(matches!(
        &message.content[0],
        tau_proto::ContentPart::Text { text } if text == "I'll inspect the file first."
    ));
    let tau_proto::ContextItem::ToolCall(call) = &items[1] else {
        panic!("expected tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_read");
    assert_eq!(call.name.as_str(), "read");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"path\":\"Cargo.toml\"}")
    );
}

#[test]
fn apply_event_completed_does_not_harvest_response_output() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    let done = apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_final",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{
                        "type": "output_text",
                        "text": "must not be harvested",
                    }],
                }],
            },
        }),
        &mut on_update,
    )
    .expect("completed");

    assert!(done);
    assert_eq!(state.response_id.as_deref(), Some("resp_final"));
    assert_eq!(state.aggregate_assistant_text(), "");
    assert!(state.into_output_items().is_empty());
}

/// Ordinary inference retains legacy `response.done` compatibility even though
/// the compact-only parser requires the documented `response.completed`.
#[test]
fn apply_event_accepts_response_done_for_ordinary_inference() {
    let mut state = path_crate_common::StreamState::new();
    let done = apply_event(
        &mut state,
        &serde_json::json!({"type": "response.done", "response": {"id": "legacy"}}),
        &mut |_| {},
    )
    .expect("ordinary response.done compatibility");

    assert!(done);
    assert_eq!(state.response_id.as_deref(), Some("legacy"));
}

#[test]
fn apply_event_completed_terminates_and_captures_response_id() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "resp_xyz",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "input_tokens_details": { "cached_tokens": 5 },
            },
        },
    });
    let done = apply_event(&mut state, &ev, &mut on_update).expect("apply ok");
    assert!(done, "response.completed must terminate the stream");
    assert_eq!(state.response_id.as_deref(), Some("resp_xyz"));
    assert_eq!(state.input_tokens, Some(42));
    assert_eq!(state.output_tokens, Some(7));
    assert_eq!(state.cached_tokens, Some(5));
}

#[test]
fn apply_event_function_call_assembles_tool_call() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};

    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_provider_item",
                "call_id": "call_a",
                "name": "shell",
                "status": "completed",
                "provider_future": { "kept": true },
            },
        }),
        &mut on_update,
    )
    .expect("ok");
    apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"cmd\":\"ls\"}",
        }),
        &mut on_update,
    )
    .expect("ok");

    let items = state.into_output_items();
    assert_eq!(items.len(), 1);
    let tau_proto::ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected function tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_a");
    assert_eq!(call.name.as_str(), "shell");
    assert_eq!(
        call.arguments,
        tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Text("cmd".into()),
            tau_proto::CborValue::Text("ls".into())
        )])
    );
    assert_eq!(call.raw_arguments_json.as_deref(), Some("{\"cmd\":\"ls\"}"));
    let envelope = call
        .responses_envelope
        .as_ref()
        .expect("responses envelope");
    assert_eq!(envelope.item_id.as_deref(), Some("fc_provider_item"));
    assert_eq!(envelope.status.as_deref(), Some("completed"));
    assert_eq!(
        envelope
            .extra_fields
            .as_ref()
            .map(crate::common::cbor_to_json),
        Some(serde_json::json!({ "provider_future": { "kept": true } }))
    );
}

#[test]
fn apply_event_failed_returns_error() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": { "message": "model overloaded" },
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result.expect_err("provider error").root_error() {
        LlmError::StreamError { body, code, .. } => {
            assert_eq!(*code, None);
            assert!(body.contains("response failed"));
            assert!(body.contains("model overloaded"));
        }
        other => panic!("expected response failure, got {other:?}"),
    }
}

/// Streaming `error` event in the documented OpenAI Responses shape:
/// `{ type: "error", code: <code>, message: <msg> }` (no nested
/// `error` object). The retry classifier needs the code in the
/// `(type=...)` suffix to distinguish account caps from transport
/// hiccups.
#[test]
fn apply_event_error_top_level_code_is_propagated() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "code": "rate_limit_exceeded",
        "message": "Rate limit reached",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result.expect_err("provider error").root_error() {
        LlmError::StreamError { body, code, .. } => {
            assert_eq!(code.as_deref(), Some("rate_limit_exceeded"));
            assert!(body.contains("Rate limit reached"));
            assert!(
                body.contains("(type=rate_limit_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
            assert!(
                crate::common::is_account_limit_body(body),
                "is_account_limit_body must classify this body as a cap"
            );
        }
        other => panic!("expected typed stream error, got {other:?}"),
    }
}

/// Nested `error.code` shape — some Codex error envelopes wrap the
/// code in an `error` object alongside the message. Must produce the
/// same suffix as the top-level form.
#[test]
fn apply_event_error_nested_code_is_propagated() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "code": "usage_limit_reached",
            "message": "The usage limit has been reached",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result.expect_err("provider error").root_error() {
        LlmError::StreamError { body, code, .. } => {
            assert_eq!(code.as_deref(), Some("usage_limit_reached"));
            assert!(body.contains("usage limit has been reached"));
            assert!(
                body.contains("(type=usage_limit_reached)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected typed stream error, got {other:?}"),
    }
}

/// Nested `error.type` shape observed from upstream — kept as a fallback so
/// captured provider events still classify correctly.
#[test]
fn apply_event_error_nested_type_fallback_is_propagated() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "error": {
            "type": "quota_exceeded",
            "message": "quota",
        },
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result.expect_err("provider error").root_error() {
        LlmError::StreamError { body, code, .. } => {
            assert_eq!(code.as_deref(), Some("quota_exceeded"));
            assert!(
                body.contains("(type=quota_exceeded)"),
                "missing (type=...) suffix in {body:?}",
            );
        }
        other => panic!("expected typed stream error, got {other:?}"),
    }
}

/// No code/type anywhere: body still produced, just without the
/// `(type=...)` suffix. The outer retry layer keeps retrying (we
/// can't safely classify), but we don't crash or drop the message.
#[test]
fn apply_event_error_without_code_omits_suffix() {
    let mut state = path_crate_common::StreamState::new();
    let mut on_update = |_: &crate::common::StreamState| {};
    let ev = serde_json::json!({
        "type": "error",
        "message": "something broke",
    });
    let result = apply_event(&mut state, &ev, &mut on_update);
    match result.expect_err("provider error").root_error() {
        LlmError::StreamError { body, code, .. } => {
            assert_eq!(*code, None);
            assert!(body.contains("something broke"));
            assert!(!body.contains("(type="), "unexpected suffix in {body:?}");
        }
        other => panic!("expected typed stream error, got {other:?}"),
    }
}

/// Reset metadata outside canonical error envelopes cannot park required work.
#[test]
fn stream_error_ignores_nested_echo_reset_hint() {
    let mut state = path_crate_common::StreamState::new();
    let event = serde_json::json!({
        "type": "error",
        "code": "overloaded_error",
        "message": "busy",
        "echo": { "resets_in_seconds": 315_360_000 }
    });
    let error = apply_event(&mut state, &event, &mut |_| {}).expect_err("stream error");
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

#[test]
fn repeated_output_text_delta_aborts_before_appending_more_output() {
    // Ensures the Responses stream guard aborts tight exact assistant text loops
    // before the repeated suffix can be emitted as a normal update.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.assistant_text_bytes() == 0);
}

#[test]
fn repeated_tool_argument_delta_aborts_before_appending_more_arguments() {
    // Ensures tool-call argument streams use the same tight exact guard, because
    // argument loops can otherwise burn the provider output budget unseen.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_output_text_done_aborts_without_appending_snapshot() {
    // Done snapshots can carry all text without prior deltas; they must be guarded
    // before becoming assistant output.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.done",
        "output_index": 0,
        "text": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.assistant_text_bytes() == 0);
}

#[test]
fn non_repeating_output_text_done_is_accepted() {
    // Non-repeating final snapshots are normal Responses events and must not be
    // rejected just because they bypassed delta streaming.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_text.done",
        "output_index": 0,
        "text": "This is a concise non-repeating answer.",
    });
    let done = apply_event(&mut state, &ev, &mut |_| {}).expect("done snapshot should apply");
    assert!(!done);
    assert_eq!(
        state.aggregate_assistant_text(),
        "This is a concise non-repeating answer."
    );
}

#[test]
fn repeated_function_arguments_done_aborts_without_appending_snapshot() {
    // Function argument done events can provide a full final argument string; the
    // guard must check it even when no argument deltas were sent.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.function_call_arguments.done",
        "output_index": 0,
        "arguments": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_custom_tool_input_done_aborts_without_appending_snapshot() {
    // Custom tool input done events share the same final-snapshot bypass risk as
    // function arguments.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.custom_tool_call_input.done",
        "output_index": 0,
        "input": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_output_item_done_message_aborts_without_appending_snapshot() {
    // Message output_item.done fallbacks are guarded in addition to the dedicated
    // output_text.done event.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": ".".repeat(1024) }]
        }
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.assistant_text_bytes() == 0);
}

#[test]
fn repeated_output_item_done_tool_arguments_abort_without_appending_snapshot() {
    // Tool output_item.done fallbacks are guarded before final arguments are
    // accepted into the tool-call accumulator.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "call_id": "call-1",
            "name": "shell",
            "arguments": "_clone".repeat(180)
        }
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}

#[test]
fn repeated_reasoning_summary_delta_aborts_before_appending() {
    // Reasoning summaries are visible stream components and need the same tight
    // exact-loop protection as assistant text.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.reasoning_summary_text.delta",
        "output_index": 0,
        "delta": ".".repeat(1024),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.thinking.is_none());
}

#[test]
fn repeated_custom_tool_input_delta_aborts_before_appending() {
    // Custom tool input deltas are guarded independently from function arguments.
    let mut state = path_crate_common::StreamState::new();
    let ev = serde_json::json!({
        "type": "response.custom_tool_call_input.delta",
        "output_index": 0,
        "delta": "_clone".repeat(180),
    });
    let result = apply_event(&mut state, &ev, &mut |_| {});
    assert!(matches!(
        result,
        Err(crate::common::LlmError::RepetitionDetected(_))
    ));
    assert!(state.output_items.is_empty());
}
/// A canonical `response.failed` context rejection preserves its code so the
/// outer logical scheduler receives a typed terminal disposition.
#[test]
fn response_failed_context_rejection_is_typed_terminal() {
    let event = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": {
                "message": "Your input exceeds the context window",
                "code": "context_length_exceeded"
            }
        }
    });
    let error = response_failed_error(
        &event,
        path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
    );
    assert_eq!(error.retry_decision(), None);
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
}

/// Ensures incomplete Responses terminals close the prompt instead of
/// repeatedly scheduling an unchanged incomplete request.
#[test]
fn response_incomplete_is_a_typed_terminal_rejection() {
    let event = serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "incomplete_details": { "reason": "max_output_tokens" }
        }
    });
    let error = response_incomplete_error(
        &event,
        path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
    );

    assert_eq!(error.retry_decision(), None);
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
}

/// Ensures only Codex's direct canonical policy and invalid-request codes
/// terminalize a failed Responses request before the scheduler sees it.
#[test]
fn response_failed_canonical_policy_codes_are_typed_terminal_rejections() {
    for code in ["cyber_policy", "invalid_prompt", "bio_policy"] {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": code,
                    "message": "request rejected"
                }
            }
        });
        let error = response_failed_error(
            &event,
            path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
        );

        assert_eq!(error.retry_decision(), None, "{code} must not retry");
        assert_eq!(
            error.failure_kind(),
            Some(tau_proto::ProviderFailureKind::RequestRejected),
            "{code} must be a typed rejection"
        );
    }
}

/// Ensures the direct legacy `type` fallback remains terminal while a direct
/// `code` keeps precedence over a conflicting fallback value.
#[test]
fn response_failed_policy_rejection_preserves_direct_code_type_precedence() {
    let typed = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": {
                "type": "cyber_policy",
                "message": "request rejected"
            }
        }
    });
    let typed_error = response_failed_error(
        &typed,
        path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
    );
    assert_eq!(
        typed_error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );

    let conflicting = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": {
                "code": "unrecognized_failure",
                "type": "bio_policy",
                "message": "request rejected"
            }
        }
    });
    let conflicting_error = response_failed_error(
        &conflicting,
        path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
    );
    assert_eq!(
        conflicting_error
            .retry_decision()
            .map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Unknown)
    );
    assert_eq!(conflicting_error.failure_kind(), None);
}

/// Prevents provider prose or nested lookalikes from impersonating the direct
/// canonical Responses error identifier that terminalizes a request.
#[test]
fn response_failed_policy_rejection_requires_direct_canonical_code() {
    for error in [
        serde_json::json!({
            "code": "unrecognized_failure",
            "message": "cyber_policy"
        }),
        serde_json::json!({
            "code": "unrecognized_failure",
            "metadata": { "code": "invalid_prompt" }
        }),
    ] {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": { "error": error }
        });
        let error = response_failed_error(
            &event,
            path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
        );

        assert_eq!(
            error.retry_decision().map(|decision| decision.class),
            Some(tau_provider::retry_policy::RetryClass::Unknown)
        );
        assert_eq!(error.failure_kind(), None);
    }
}

/// Ensures the exact current Codex overload identifier selects overload retry,
/// while prose and nested lookalikes remain unknown rather than authoritative.
#[test]
fn response_failed_overload_code_requires_direct_canonical_identifier() {
    let canonical = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": {
                "code": "server_is_overloaded",
                "message": "try later"
            }
        }
    });
    let canonical_error = response_failed_error(
        &canonical,
        path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
    );
    assert_eq!(
        canonical_error
            .retry_decision()
            .map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Overload)
    );

    for error in [
        serde_json::json!({
            "code": "unrecognized_failure",
            "message": "server_is_overloaded"
        }),
        serde_json::json!({
            "code": "unrecognized_failure",
            "metadata": { "code": "server_is_overloaded" }
        }),
    ] {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": { "error": error }
        });
        let error = response_failed_error(
            &event,
            path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
        );
        assert_eq!(
            error.retry_decision().map(|decision| decision.class),
            Some(tau_provider::retry_policy::RetryClass::Unknown)
        );
    }
}
