use std::fs::OpenOptions;

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

/// Append timing must report exact content-free EOF boundaries so slow-write
/// diagnostics can distinguish file position from current-line size.
#[test]
fn append_timing_reports_exact_eof_boundaries() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("timing.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open log");
    let first_line = b"{\"type\":\"first\"}\n";
    let second_line = b"{\"type\":\"second\"}\n";
    let first = append_line(&mut file, first_line).expect("append first");
    let second = append_line(&mut file, second_line).expect("append second");
    let first_end = first_line.len() as u64;
    let second_end = first_end + second_line.len() as u64;

    assert_eq!(first.start_offset, Some(0));
    assert_eq!(first.end_offset, Some(first_end));
    assert_eq!(second.start_offset, first.end_offset);
    assert_eq!(second.end_offset, Some(second_end));
    assert_eq!(first.rollback, Duration::ZERO);
    assert_eq!(second.rollback, Duration::ZERO);
}

/// A failed append with successful rollback reports the exact restored EOF.
#[test]
fn append_timing_reports_clean_rollback_boundary() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("timing.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log");
    let line = b"{\"type\":\"failed\"}\n";
    let error = append_line(
        &mut FaultInjectingFile::new(
            &mut file,
            AppendFault {
                fail_write_at: Some(1),
                ..AppendFault::default()
            },
            line.len(),
        ),
        line,
    )
    .expect_err("append should fail");

    assert_eq!(error.timing.start_offset, Some(0));
    assert_eq!(error.timing.end_offset, Some(0));
}

/// Uncertain rollback reports no end boundary so slow-cycle diagnostics do not
/// imply that the failed line was removed.
#[test]
fn append_timing_omits_uncertain_rollback_boundary() {
    let td = tempfile::tempdir().expect("tempdir");
    let path = td.path().join("timing.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open log");
    let line = b"{\"type\":\"failed\"}\n";
    let error = append_line(
        &mut FaultInjectingFile::new(
            &mut file,
            AppendFault {
                fail_write_at: Some(1),
                fail_truncate: true,
                ..AppendFault::default()
            },
            line.len(),
        ),
        line,
    )
    .expect_err("append should fail");

    assert_eq!(error.timing.start_offset, Some(0));
    assert_eq!(error.timing.end_offset, None);
    assert!(error.rollback.is_some());
}

/// Every possible line-byte failure, including the final newline, rolls back to
/// a parseable prefix and permits a later complete append.
#[test]
fn line_byte_failures_roll_back_and_allow_later_append() {
    let rejected = serde_json::json!({
        "type": "published",
        "event_name": "provider.tool_result",
        "event": {"output": "diagnostic payload"},
    });
    let mut encoded = serde_json::to_vec(&rejected).expect("serialize expected line");
    encoded.push(b'\n');

    for fail_write_at in 0..encoded.len() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut log = DebugEventLog::open(td.path()).expect("open");
        log.write_entry(&serde_json::json!({"type": "baseline"}))
            .expect("write baseline");
        let baseline = std::fs::read(log.path()).expect("read baseline");
        log.inject_fault(AppendFault {
            fail_write_at: Some(fail_write_at),
            ..AppendFault::default()
        });

        let error = log
            .write_entry(&rejected)
            .expect_err("injected line write must fail");

        assert!(error.should_report());
        assert_eq!(
            std::fs::read(log.path()).expect("read rolled-back log"),
            baseline,
            "failure before line byte {fail_write_at} retained a fragment"
        );
        assert_eq!(read_lines(log.path()).len(), 1);
        log.write_entry(&serde_json::json!({"type": "after_rollback"}))
            .expect("append after successful rollback");
        assert_eq!(read_lines(log.path()).len(), 2);
    }
}

/// A failed commit flush removes the fully written line and keeps the log
/// appendable under the existing `Write::flush` policy.
#[test]
fn commit_flush_failure_rolls_back_complete_line() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    log.write_entry(&serde_json::json!({"type": "baseline"}))
        .expect("write baseline");
    let baseline = std::fs::read(log.path()).expect("read baseline");
    log.inject_fault(AppendFault {
        fail_commit_flush: true,
        ..AppendFault::default()
    });

    log.write_entry(&serde_json::json!({"type": "rejected"}))
        .expect_err("commit flush must fail");

    assert_eq!(
        std::fs::read(log.path()).expect("read rolled-back log"),
        baseline
    );
    log.write_entry(&serde_json::json!({"type": "retry"}))
        .expect("append after flush rollback");
    assert_eq!(read_lines(log.path()).len(), 2);
}

/// Either rollback operation failing poisons the writer, leaves later attempts
/// byte-for-byte inert, and makes only the poisoning failure reportable.
#[test]
fn rollback_failure_disables_later_writes_and_reports_once() {
    for fault in [
        AppendFault {
            fail_write_at: Some(3),
            fail_truncate: true,
            ..AppendFault::default()
        },
        AppendFault {
            fail_write_at: Some(3),
            fail_rollback_flush: true,
            ..AppendFault::default()
        },
        AppendFault {
            fail_commit_flush: true,
            fail_truncate: true,
            ..AppendFault::default()
        },
        AppendFault {
            fail_commit_flush: true,
            fail_rollback_flush: true,
            ..AppendFault::default()
        },
    ] {
        let td = tempfile::tempdir().expect("tempdir");
        let mut log = DebugEventLog::open(td.path()).expect("open");
        log.inject_fault(fault);

        let poisoning_error = log
            .write_entry(&serde_json::json!({"type": "poison"}))
            .expect_err("rollback uncertainty must fail");
        let bytes_after_failure = std::fs::read(log.path()).expect("read poisoned log");
        let disabled_error = log
            .write_entry(&serde_json::json!({"type": "must_not_append"}))
            .expect_err("poisoned writer stays disabled");

        assert!(poisoning_error.should_report());
        assert!(!disabled_error.should_report());
        assert!(poisoning_error.bounded_diagnostic().chars().count() <= 257);
        assert_eq!(
            std::fs::read(log.path()).expect("read untouched poisoned log"),
            bytes_after_failure
        );
    }
}

/// Diagnostics retain a fixed 256-character body and one ellipsis even when an
/// operating-system error string is unexpectedly large.
#[test]
fn debug_log_diagnostic_is_bounded_at_documented_limit() {
    let error = DebugLogError::Append {
        source: std::io::Error::other("x".repeat(DEBUG_LOG_DIAGNOSTIC_CHARS * 2)),
        rollback: None,
    };

    let diagnostic = error.bounded_diagnostic();

    assert_eq!(diagnostic.chars().count(), DEBUG_LOG_DIAGNOSTIC_CHARS + 1);
    assert!(diagnostic.ends_with('…'));
}

/// Raw harness events and published events share the same failure-atomic append
/// path and expose failures to their harness caller.
#[test]
fn harness_and_published_logging_observe_append_failures_consistently() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut harness_log =
        DebugEventLog::open(&td.path().join("harness")).expect("open harness log");
    harness_log.inject_fault(AppendFault {
        fail_write_at: Some(1),
        ..AppendFault::default()
    });
    harness_log
        .log_harness_event(&HarnessEvent::Disconnected {
            connection_id: ConnectionId::from("conn-1"),
        })
        .expect_err("raw harness append failure is observable");
    assert!(read_lines(harness_log.path()).is_empty());

    let mut published_log =
        DebugEventLog::open(&td.path().join("published")).expect("open published log");
    published_log.inject_fault(AppendFault {
        fail_write_at: Some(1),
        ..AppendFault::default()
    });
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::parse("sp-0")
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    });
    published_log
        .log_published_event(None, &event, UnixMicros::now())
        .expect_err("published append failure is observable");
    assert!(read_lines(published_log.path()).is_empty());
}

#[test]
fn published_line_preserves_enriched_token_usage() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let model: ModelId = "openai/gpt-5".parse().expect("model id");
    let event = Event::ProviderResponseFinished(ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: AgentPromptId::parse("sp-0")
            .expect("known-safe AgentPromptId must be valid"),
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
            prompt_cache_read_ceiling_tokens: None,
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
    )
    .expect("log published event");

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
        agent_prompt_id: AgentPromptId::parse("sp-0")
            .expect("known-safe AgentPromptId must be valid"),
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

    log.log_published_event(None, &event, UnixMicros::now())
        .expect("log published event");

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
        session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        extension_name: ExtensionName::from("tau-ext-pim"),
        instance_id: ExtensionInstanceId::from(0),
        action_id: "email.auth.google.finish".to_owned(),
        raw_line: ":email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret".to_owned(),
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

    log.log_published_event(None, &event, UnixMicros::now())
        .expect("log published event");

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

/// Full provider prompts become a bounded content-free summary before JSON
/// conversion, so neither text nor image bytes enter the debug mirror.
#[test]
fn full_prompt_debug_projection_is_fixed_shape_and_content_free() {
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
        agent_prompt_id: "ap-main-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        system_prompt: "unique-system-secret".repeat(1_000),
        context: tau_proto::PromptContext {
            blocks: vec![
                tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                    items: vec![
                        tau_proto::ContextItem::Message(tau_proto::MessageItem {
                            role: tau_proto::ContextRole::User,
                            content: vec![tau_proto::ContentPart::Text {
                                text: "unique-context-secret".to_owned(),
                            }],
                            phase: None,
                            responses_raw_json: None,
                        }),
                        tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                            kind: tau_proto::ReasoningTextKind::Summary,
                            text: "unique-reasoning-secret".to_owned(),
                        }),
                        tau_proto::ContextItem::UnknownProviderItem(
                            tau_proto::OpaqueProviderItem::with_raw_json(
                                CborValue::Text("unique-opaque-secret".to_owned()),
                                "unique-raw-json-secret".to_owned(),
                            ),
                        ),
                    ],
                }),
                tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                    items: vec![tool_result],
                }),
            ],
        },
        tools: vec![tau_proto::ToolDefinition {
            name: tau_proto::ToolName::new("unique_tool"),
            model_visible_name: None,
            description: Some("unique-tool-description-secret".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({"unique-schema-secret": {"type": "string"}})),
            format: None,
        }],
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
    assert!(!rendered.contains("unique-system-secret"));
    for sentinel in [
        "unique-context-secret",
        "unique-reasoning-secret",
        "unique-opaque-secret",
        "unique-raw-json-secret",
        "unique_tool",
        "unique-tool-description-secret",
        "unique-schema-secret",
    ] {
        assert!(!rendered.contains(sentinel), "{sentinel} leaked");
    }
    assert!(
        rendered.len() < 1_024,
        "prompt debug summary must stay bounded"
    );
    assert_eq!(
        json,
        serde_json::json!({
            "event": "agent.prompt_created",
            "payload": {
                "agent_prompt_id": "ap-main-1",
                "agent_id": "main",
                "session_id": "s1",
                "model": "chatgpt/gpt-5.6-sol",
                "operation": "inference",
                "summary": {
                    "system_prompt_utf8_bytes": "unique-system-secret".len() * 1_000,
                    "context_blocks": 2,
                    "context_items": 4,
                    "context_text_utf8_bytes":
                        "unique-context-secret".len() + "unique-reasoning-secret".len(),
                    "provider_images": 1,
                    "provider_image_bytes": sentinel.len(),
                    "tools": 1,
                },
            },
        })
    );

    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    log.log_harness_event(&HarnessEvent::FromConnection {
        connection_id: "interceptor".into(),
        message: Box::new(HarnessInputMessage::InterceptReply(
            tau_proto::InterceptReply {
                action: tau_proto::InterceptAction::Pass(Some(Box::new(event))),
            },
        )),
    })
    .expect("log intercepted full prompt summary");
    let raw = std::fs::read_to_string(log.path()).expect("read debug log");
    for sentinel in [
        "unique-system-secret",
        "unique-context-secret",
        "unique-reasoning-secret",
        "unique-opaque-secret",
        "unique-raw-json-secret",
        "unique_tool",
        "unique-tool-description-secret",
        "unique-schema-secret",
    ] {
        assert!(!raw.contains(sentinel), "{sentinel} leaked from raw reply");
    }
    assert!(raw.contains("\"summary\""));
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
        agent_prompt_id: AgentPromptId::parse("sp-0")
            .expect("known-safe AgentPromptId must be valid"),
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
    })
    .expect("skip transient harness event");

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
    })
    .expect("skip UI debug stats request");

    assert!(read_lines(log.path()).is_empty());
}

/// Ensures raw terminal provider reports cannot leak embedded provider-image
/// bytes into debug JSONL.
#[test]
fn provider_finished_report_clears_image_bytes_before_debug_serialization() {
    let mut event = Event::ProviderResponseFinishedReported(ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: AgentPromptId::parse("sp-image")
            .expect("known-safe AgentPromptId must be valid"),
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
