use tau_proto::{
    ActionInvocationId, AgentPromptId, CborValue, ExtensionInstanceId, ExtensionName,
    HarnessInputMessage, ModelId, PromptOriginator, ProviderResponseFinished,
    ProviderResponseTextDelta, ProviderResponseUpdated, ProviderTokenUsage, ReasoningTextKind,
    SessionId,
};

use super::*;
use crate::event::HarnessEvent;

fn read_lines(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).expect("read events.jsonl");
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("parse line"))
        .collect()
}

#[test]
fn published_line_preserves_enriched_token_usage() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let model: ModelId = "openai/gpt-5".parse().expect("model id");
    let event = Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: Some(tau_proto::ContextLimitTelemetry {
            model: model.clone(),
            operation: tau_proto::PromptOperation::Inference,
            projected_input_tokens: Some(999),
            transcript_delta_bytes: Some(99),
            advertised_context_window: Some(1024),
            provider_input_tokens: Some(1000),
            projection_reserve_tokens: 4096,
            compaction_threshold: None,
            compaction_policy: tau_proto::ContextLimitCompactionPolicy::ProviderDefault,
            recovery_eligible: false,
            action: tau_proto::ContextLimitAction::Terminal,
            observation: tau_proto::ContextLimitObservation::RejectedBelowAdvertisedLimit,
        }),
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: PromptOriginator::User,
        usage: Some(ProviderTokenUsage {
            model: Some(model),
            prompt_sent_tokens: 1000,
            prompt_cached_tokens: 800,
            response_received_tokens: 42,
            stats: tau_proto::TokenUsageStats::default(),
        }),
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    });
    log.log_published_event(
        Some(&ConnectionId::from("conn-1")),
        &event,
        UnixMicros::now(),
    );

    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["type"], "published");
    assert_eq!(line["event_name"], "provider.response_finished");
    assert_eq!(line["source"], "conn-1");
    let usage = &line["event"]["payload"]["usage"];
    assert_eq!(usage["prompt_sent_tokens"], 1000);
    assert_eq!(usage["prompt_cached_tokens"], 800);
    assert_eq!(usage["response_received_tokens"], 42);
    assert_eq!(usage["model"], "openai/gpt-5");
    assert_eq!(line["event"]["payload"]["agent_id"], "main");
    assert_eq!(line["event"]["payload"]["agent_prompt_id"], "sp-0");
    assert_eq!(
        line["event"]["payload"]["context_limit_telemetry"]["transcript_delta_bytes"],
        99
    );
}

#[test]
fn published_line_compacts_long_strings() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![
            ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "x".repeat(101),
                phase: None,
            },
            ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: ReasoningTextKind::Summary,
                text: format!("{}{}{}", "α".repeat(30), "middle", "ω".repeat(30)),
            },
        ],
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    });

    log.log_published_event(None, &event, UnixMicros::now());

    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 1);
    let payload = &lines[0]["event"]["payload"];
    assert_eq!(
        payload["deltas"][0]["text"],
        "xxxxxxxxxxxxxxxxxxxx┄total 101┄xxxxxxxxxxxxxxxxxxxx"
    );
    assert_eq!(
        payload["deltas"][1]["text"],
        "αααααααααα┄total 126┄ωωωωωωωωωω"
    );
}

#[test]
fn published_action_invoke_redacts_gmail_oauth_redirect_url() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: ActionInvocationId::from("action-1"),
        session_id: SessionId::from("s1"),
        extension_name: ExtensionName::from("tau-ext-pim"),
        instance_id: ExtensionInstanceId::from(0),
        action_id: "email.auth.google.finish".to_owned(),
        raw_line: "/email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret".to_owned(),
        argv: vec![
            "work".to_owned(),
            "http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret".to_owned(),
        ],
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("account".to_owned()),
                CborValue::Text("work".to_owned()),
            ),
            (
                CborValue::Text("redirect_url".to_owned()),
                CborValue::Text(
                    "http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret"
                        .to_owned(),
                ),
            ),
        ]),
    });

    log.log_published_event(None, &event, UnixMicros::now());

    let raw = std::fs::read_to_string(log.path()).expect("read events.jsonl");
    assert!(!raw.contains("auth-code-secret"));
    assert!(!raw.contains("state-secret"));
    assert!(raw.contains("<redirect-url-redacted>"));
    assert!(raw.contains("\"arguments\":\"<redacted>\""));
}

#[test]
fn compact_debug_string_keeps_short_strings() {
    assert_eq!(compact_debug_string(&"x".repeat(100)), "x".repeat(100));
}

/// Ensures image bytes nested in later prompt contexts are removed before JSON
/// conversion, rather than expanding into decimal byte arrays in debug logs.
#[test]
fn nested_provider_image_bytes_are_redacted_before_debug_json() {
    let sentinel = b"\x89PNG\r\n\x1a\nunique-image-sentinel".to_vec();
    let tool_result = tau_proto::ToolResultItem {
        call_id: "call-image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("image metadata".to_owned())),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: sentinel.clone().into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
    };
    let event = Event::AgentPromptCreated(tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-main-1".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: SessionId::from("s1"),
        system_prompt: "system".to_owned(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::ToolResults(
                tau_proto::ToolResultsBlock {
                    items: vec![tool_result],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: "chatgpt/gpt-5.6-sol".parse().expect("model"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        originator: PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    });

    let json = debug_event_json(&event);
    let rendered = serde_json::to_string(&json).expect("render debug JSON");
    assert!(!rendered.contains("unique-image-sentinel"));
    assert_eq!(
        json["payload"]["context"]["blocks"][0]["payload"]["items"][0]["provider_content"][0]["content"]
            ["data"],
        serde_json::json!([])
    );
}

/// Ensures committed terminal result reports remain observable in debug JSON
/// while their typed provider-image bytes are cleared before serialization.
#[test]
fn tool_result_report_image_bytes_are_redacted_before_debug_json() {
    let event = Event::ToolResultReported(tau_proto::ToolResult {
        call_id: "call-image-report".into(),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("safe image metadata".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: b"\x89PNG\r\n\x1a\nreport-image-sentinel".to_vec().into(),
                width: 640,
                height: 480,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    });

    let json = debug_event_json(&event);
    let rendered = serde_json::to_string(&json).expect("render debug JSON");
    assert!(rendered.contains("tool.result_reported"));
    assert!(rendered.contains("safe image metadata"));
    assert!(!rendered.contains("report-image-sentinel"));
    assert_eq!(
        json["payload"]["provider_content"][0]["content"]["data"],
        serde_json::json!([])
    );
    assert_eq!(
        json["payload"]["provider_content"][0]["content"]["width"],
        640
    );
}

/// Ensures interceptor replies cannot bypass incoming-frame image redaction by
/// nesting a replacement event outside `HarnessInputMessage::Emit`.
#[test]
fn intercept_reply_nested_event_redacts_provider_image_bytes() {
    let mut message = HarnessInputMessage::InterceptReply(tau_proto::InterceptReply {
        action: tau_proto::InterceptAction::Pass(Some(Box::new(Event::ProviderToolResult(
            tau_proto::ToolResult {
                call_id: "call-image".into(),
                tool_name: tau_proto::ToolName::new("read_image"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("metadata".to_owned()),
                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                    tau_proto::ImageContent {
                        media_type: tau_proto::ImageMediaType::Png,
                        data: b"\x89PNG\r\n\x1a\nsentinel".to_vec().into(),
                        width: 1,
                        height: 1,
                        detail: tau_proto::ImageDetail::High,
                    },
                )],
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            },
        )))),
    });

    redact_harness_input_message_binary_content(&mut message);
    let HarnessInputMessage::InterceptReply(reply) = message else {
        panic!("intercept reply");
    };
    let tau_proto::InterceptAction::Pass(Some(event)) = reply.action else {
        panic!("replacement event");
    };
    let Event::ProviderToolResult(result) = *event else {
        panic!("provider tool result");
    };
    let tau_proto::ToolResultContentPart::Image(image) = &result.provider_content[0];
    assert!(image.data.is_empty());
}

#[test]
fn transient_from_connection_events_are_not_logged_twice() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::from("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "partial".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    });

    log.log_harness_event(&HarnessEvent::FromConnection {
        connection_id: ConnectionId::from("conn-1"),
        message: Box::new(HarnessInputMessage::emit(event)),
    });

    let lines = read_lines(log.path());
    assert!(
        lines.is_empty(),
        "transient streaming events are logged on publish; the raw inbound copy is redundant"
    );
}

/// Dedicated UI debug-stat requests stay out of debug JSONL just as the
/// superseded transient request event did.
#[test]
fn ui_debug_event_stats_requests_are_not_logged() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");

    log.log_harness_event(&HarnessEvent::FromConnection {
        connection_id: ConnectionId::from("ui"),
        message: Box::new(HarnessInputMessage::UiDebugEventStatsRequest(
            tau_proto::UiDebugEventStatsRequest {
                extension_name: "secret-extension".into(),
            },
        )),
    });

    assert!(read_lines(log.path()).is_empty());
}

/// Ensures raw terminal provider reports cannot leak embedded provider-image
/// bytes into debug JSONL.
#[test]
fn provider_finished_report_clears_image_bytes_before_debug_serialization() {
    let mut event = Event::ProviderResponseFinishedReported(ProviderResponseFinished {
        agent_prompt_id: AgentPromptId::from("sp-image"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![tau_proto::ContextItem::ToolResult(
            tau_proto::ToolResultItem {
                call_id: "call-image".into(),
                tool_type: tau_proto::ToolType::Function,
                status: tau_proto::ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("image".into())),
                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                    tau_proto::ImageContent {
                        media_type: tau_proto::ImageMediaType::Png,
                        data: vec![1, 2, 3].into(),
                        width: 1,
                        height: 1,
                        detail: tau_proto::ImageDetail::High,
                    },
                )],
            },
        )],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    });

    redact_event_binary_content(&mut event);
    let Event::ProviderResponseFinishedReported(finished) = event else {
        unreachable!("report variant")
    };
    let tau_proto::ContextItem::ToolResult(result) = &finished.output_items[0] else {
        unreachable!("tool result")
    };
    let tau_proto::ToolResultContentPart::Image(image) = &result.provider_content[0];
    assert!(image.data.is_empty());
}
