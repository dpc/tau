use std::fs::OpenOptions;
use std::io as path_std_io;

use tau_proto::{
    ActionInvocationId, AgentPromptId, CborValue, ExtensionInstanceId, HarnessInputMessage,
    ModelId, PromptOriginator, ProviderResponseFinished, ProviderResponseTextDelta,
    ProviderResponseUpdated, ProviderTokenUsage, ReasoningTextKind, SessionId,
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

/// Bootstrap sensitivity removes exact queued prompt bytes while the ordinary
/// projection retains them for diagnostics.
#[test]
fn bootstrap_prompt_sensitivity_is_narrow_and_content_free() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open debug log");
    let secret = "bootstrap-secret-7f3cb88b";
    let event = Event::AgentPromptQueued(tau_proto::AgentPromptQueued {
        agent_id: tau_proto::AgentId::parse("bootstrap-agent").expect("agent id"),
        text: secret.to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
    });

    log.log_published_event(None, &event, UnixMicros::now())
        .expect("write ordinary projection");
    log.log_published_event_with_sensitivity(
        None,
        &event,
        UnixMicros::now(),
        DebugEventSensitivity::BootstrapPrompt,
    )
    .expect("write sensitive projection");

    let lines = read_lines(log.path());
    let ordinary = serde_json::to_string(&lines[0]).expect("serialize ordinary row");
    let sensitive = serde_json::to_string(&lines[1]).expect("serialize sensitive row");
    assert!(ordinary.contains(secret));
    assert!(!sensitive.contains(secret));
    assert!(sensitive.contains("<bootstrap-prompt-redacted>"));
}

/// Reproduces the former generic input projection for regression comparisons.
fn legacy_debug_harness_input_json(message: &HarnessInputMessage) -> serde_json::Value {
    let mut redacted = message.clone();
    redact_harness_input_message_binary_content(&mut redacted);
    serde_json::to_value(redacted).unwrap_or_default()
}

/// Reproduces the former generic published-event projection for regression
/// comparisons.
fn legacy_debug_event_json(event: &Event) -> serde_json::Value {
    if let Event::AgentPromptCreated(prompt) = event {
        return prompt_created_debug_summary(prompt);
    }
    if let Event::ProviderResponseUpdated(updated) | Event::ProviderResponseUpdatedReported(updated) =
        event
        && updated
            .status
            .as_ref()
            .and_then(|status| status.retry.as_ref())
            .is_some()
    {
        return provider_retry_debug_projection(event.name(), updated);
    }
    let mut redacted = event.clone();
    redact_event_binary_content(&mut redacted);
    serde_json::to_value(redacted).unwrap_or_default()
}

/// Applies the established string compaction before comparing complete JSON
/// rows.
fn compacted_json_bytes(value: serde_json::Value) -> Vec<u8> {
    let mut value = value;
    compact_debug_json_strings(&mut value);
    serde_json::to_vec(&value).expect("serialize compacted debug JSON")
}

fn debug_provider_finished(output_items: Vec<tau_proto::ContextItem>) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: AgentPromptId::parse("sp-debug").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items,
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn debug_image_tool_result(data: Vec<u8>) -> tau_proto::ToolResult {
    tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("safe image metadata".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: data.into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    }
}

fn debug_image_context_item(data: Vec<u8>) -> tau_proto::ContextItem {
    tau_proto::ContextItem::ToolResult(tau_proto::ToolResultItem {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_type: tau_proto::ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("image".into())),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: data.into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
    })
}

fn debug_binary_events(data: Vec<u8>) -> Vec<Event> {
    let compacted = tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        compact_prompt_id: None,
        model: None,
        operation: None,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        transaction_id: None,
        cut: None,
        suffix_end: None,
        replacement_window: vec![debug_image_context_item(data.clone())],
    };
    let finished = debug_provider_finished(vec![debug_image_context_item(data.clone())]);
    vec![
        Event::ToolResultReported(debug_image_tool_result(data.clone())),
        Event::ToolResult(debug_image_tool_result(data.clone())),
        Event::ProviderToolResult(debug_image_tool_result(data.clone())),
        Event::AgentCompacted(compacted),
        Event::ProviderResponseFinishedReported(finished.clone()),
        Event::ProviderResponseFinished(finished),
    ]
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
        source: path_std_io::Error::other("x".repeat(DEBUG_LOG_DIAGNOSTIC_CHARS * 2)),
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
            connection_id: tau_proto::ConnectionId::parse("conn-1")
                .expect("test connection id must satisfy the identifier grammar"),
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
        automatic_compaction_decision: None,
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
            transcript_delta_bytes: Some(tau_proto::ByteCount::new(99)),
            advertised_context_window: Some(tau_proto::TokenCount::new(1024)),
            provider_input_tokens: Some(tau_proto::TokenCount::new(1000)),
            compaction_threshold: None,
            compaction_policy: tau_proto::ContextLimitCompactionPolicy::ProviderDefault,
            recovery_eligible: false,
            action: tau_proto::ContextLimitAction::Terminal,
            observation: tau_proto::ContextLimitObservation::RejectedBelowAdvertisedLimit,
        }),
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: Some(ProviderTokenUsage {
            model: Some(model),
            prompt_sent_tokens: 1000,
            prompt_cached_tokens: 800,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: 42,
            stats: tau_proto::TokenUsageStats::default(),
        }),
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    });
    log.log_published_event(
        Some(
            &tau_proto::ConnectionId::parse("conn-1")
                .expect("test connection id must satisfy the identifier grammar"),
        ),
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

/// Ensures best-effort published draft rows inherit the wire contract: default
/// liveness rows omit prompt text, while explicitly supplied content remains
/// available for diagnostic use.
#[test]
fn published_prompt_draft_rows_omit_or_preserve_opt_in_content() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    let session_id = SessionId::parse("s1").expect("session id");
    let absent = Event::UiPromptDraft(tau_proto::UiPromptDraft {
        session_id: session_id.clone(),
        target_agent_id: None,
        text: None,
    });
    let contentful = Event::UiPromptDraft(tau_proto::UiPromptDraft {
        session_id,
        target_agent_id: None,
        text: Some("explicit content".to_owned()),
    });

    log.log_published_event(None, &absent, UnixMicros::now())
        .expect("log content-free draft");
    log.log_published_event(None, &contentful, UnixMicros::now())
        .expect("log contentful draft");

    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 2);
    assert!(lines[0]["event"]["payload"].get("text").is_none());
    assert_eq!(lines[1]["event"]["payload"]["text"], "explicit content");
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
        invocation_id: ActionInvocationId::parse("action-1").expect("test identifier must be valid"),
        session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        extension_name: tau_proto::ExtensionName::parse("tau-ext-pim").expect("test extension name must satisfy the identifier grammar"),
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

/// Ordinary submitted prompts retain the former byte-exact debug JSON while the
/// projection borrows the original message instead of cloning its prompt text.
#[test]
fn submitted_prompt_debug_projection_borrows_and_matches_legacy_json() {
    let text = format!("prefix-{}-suffix", "prompt-body-".repeat(1_000));
    let message =
        HarnessInputMessage::emit(Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            session_id: SessionId::parse("s1").expect("session id"),
            text,
            literal: true,
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            message_class: Default::default(),
            originator: PromptOriginator::User,
            ctx_id: Some("ctx-1".to_owned()),
        }));

    let projection = debug_harness_input_projection(&message);
    let DebugHarnessInputProjection::Borrowed(borrowed) = projection else {
        panic!("ordinary prompt must not require an owned redaction copy");
    };
    assert!(
        std::ptr::eq(borrowed, &message),
        "borrowed projection must retain the inbound message identity"
    );

    let legacy = legacy_debug_harness_input_json(&message);
    let current = debug_harness_input_json(&message);
    assert_eq!(
        serde_json::to_vec(&current).expect("serialize current JSON"),
        serde_json::to_vec(&legacy).expect("serialize legacy JSON")
    );
    assert_eq!(
        compacted_json_bytes(current),
        compacted_json_bytes(legacy),
        "bounded debug output must remain byte-exact"
    );
}

/// Published canonical events without image bytes borrow their original values,
/// so large prompts and ordinary provider updates/terminals avoid the former
/// full-event clone without changing their compacted debug JSON.
#[test]
fn published_binary_free_events_borrow_and_match_legacy_json() {
    let submitted = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        inference_activation: true,
        text: format!("prefix-{}-suffix", "submitted-prompt-body-".repeat(1_000)),
        trusted_internal_spans: Vec::new(),
        message_class: Default::default(),
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    });
    let updated = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::parse("sp-ordinary").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "ordinary public delta".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    });
    let finished = Event::ProviderResponseFinished(debug_provider_finished(Vec::new()));

    for event in [&submitted, &updated, &finished] {
        let DebugEventProjection::Borrowed(borrowed) = debug_event_projection(event) else {
            panic!("binary-free published event must not require an owned redaction copy");
        };
        assert!(
            std::ptr::eq(borrowed, event),
            "borrowed published projection must retain event identity"
        );

        let current = debug_event_json(event);
        let legacy = legacy_debug_event_json(event);
        assert_eq!(
            serde_json::to_vec(&current).expect("serialize current JSON"),
            serde_json::to_vec(&legacy).expect("serialize legacy JSON"),
            "published debug JSON changed for {}",
            event.name()
        );
        assert_eq!(
            compacted_json_bytes(current),
            compacted_json_bytes(legacy),
            "bounded published debug JSON changed for {}",
            event.name()
        );
    }
}

/// Provider projections compact large borrowed strings before the JSON tree
/// owns them while preserving the former byte-exact bounded representation.
#[test]
fn provider_projection_compacts_borrowed_unicode_and_raw_sidecars_byte_exactly() {
    let middle_canary = "PROVIDER-PRIVATE-MIDDLE-CANARY";
    let large = format!(
        "prefix🙂{}{}終端",
        middle_canary,
        "reasoning\\\"\n🙂".repeat(1_000)
    );
    let raw = serde_json::json!({
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": large.clone()}],
    })
    .to_string();
    let opaque =
        tau_proto::OpaqueProviderItem::from_raw_json(raw).expect("valid opaque reasoning fixture");
    let updated = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::parse("sp-large-update").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![ProviderResponseTextDelta::Message {
            output_index: 0,
            text: large.clone(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    });
    let finished = Event::ProviderResponseFinished(debug_provider_finished(vec![
        tau_proto::ContextItem::Reasoning(opaque),
        tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: ReasoningTextKind::Summary,
            text: large,
        }),
    ]));

    for event in [&updated, &finished] {
        let current = debug_event_json(event);
        let current_bytes = serde_json::to_vec(&current).expect("serialize current projection");
        assert_eq!(
            current_bytes,
            compacted_json_bytes(legacy_debug_event_json(event)),
            "borrowed Provider projection changed legacy bytes for {}",
            event.name()
        );
        let rendered = String::from_utf8(current_bytes).expect("JSON is UTF-8");
        assert!(
            !rendered.contains(middle_canary),
            "large Provider content crossed the bounded projection"
        );
        assert!(rendered.contains("┄total "));
    }
}

/// Every event shape selected by the shared image classifier takes the owned
/// path and clears bytes before published JSON serialization, matching the
/// former clone-and-redact projection exactly.
#[test]
fn published_binary_events_redact_every_classified_nested_image() {
    let image_bytes = b"published-debug-image-sentinel".to_vec();
    let serialized_bytes =
        serde_json::to_string(&image_bytes).expect("serialize original image bytes");

    for event in debug_binary_events(image_bytes) {
        assert!(
            matches!(
                debug_event_projection(&event),
                DebugEventProjection::Redacted(_)
            ),
            "{} with image bytes must own a redacted debug projection",
            event.name()
        );
        let current = debug_event_json(&event);
        let rendered = serde_json::to_string(&current).expect("render debug JSON");
        assert!(
            !rendered.contains(&serialized_bytes),
            "{} retained original image bytes in debug JSON",
            event.name()
        );
        assert!(
            rendered.contains("\"data\":[]"),
            "{} did not retain its byte-free image shape",
            event.name()
        );
        assert_eq!(
            compacted_json_bytes(current),
            compacted_json_bytes(legacy_debug_event_json(&event)),
            "bounded published debug JSON changed for {}",
            event.name()
        );
    }
}

/// Empty image buffers serialize identically without redaction, so every shared
/// classifier shape borrows its original event rather than allocating a copy.
#[test]
fn published_empty_binary_events_borrow_and_match_legacy_json() {
    for event in debug_binary_events(Vec::new()) {
        let DebugEventProjection::Borrowed(borrowed) = debug_event_projection(&event) else {
            panic!(
                "{} with an empty image must not require an owned redaction copy",
                event.name()
            );
        };
        assert!(
            std::ptr::eq(borrowed, &event),
            "borrowed published projection must retain event identity"
        );
        assert_eq!(
            compacted_json_bytes(debug_event_json(&event)),
            compacted_json_bytes(legacy_debug_event_json(&event)),
            "bounded published debug JSON changed for empty {}",
            event.name()
        );
    }
}

/// A large malformed-config diagnostic remains borrowed and receives the same
/// bounded debug JSON compaction as the previous clone-and-redact path.
#[test]
fn large_malformed_config_error_debug_projection_matches_legacy_json() {
    let message = HarnessInputMessage::ConfigError(tau_proto::ConfigError {
        message: format!("invalid config: \0{}: unexpected token", "x".repeat(1_024)),
    });

    assert!(matches!(
        debug_harness_input_projection(&message),
        DebugHarnessInputProjection::Borrowed(_)
    ));
    assert_eq!(
        compacted_json_bytes(debug_harness_input_json(&message)),
        compacted_json_bytes(legacy_debug_harness_input_json(&message)),
        "malformed diagnostic projection changed"
    );
}

/// Keeps strings at or below the byte threshold in their original JSON
/// allocation while compacting only longer strings with the established marker.
#[test]
fn compact_debug_json_strings_keeps_short_allocations_and_compacts_long_strings() {
    let ascii_99 = "a".repeat(99);
    let ascii_100 = "b".repeat(100);
    let unicode_100 = "é".repeat(50);
    let unicode_split_edges = format!("{}é{}é{}", "a".repeat(19), "b".repeat(60), "c".repeat(19));
    let mut value = serde_json::json!({
        "ascii_99": ascii_99,
        "ascii_100": ascii_100,
        "ascii_101": "c".repeat(101),
        "unicode_100": unicode_100,
        "unicode_102": "é".repeat(51),
        "unicode_split_edges": unicode_split_edges,
    });

    let ascii_99_ptr = value["ascii_99"].as_str().expect("string").as_ptr();
    let ascii_100_ptr = value["ascii_100"].as_str().expect("string").as_ptr();
    let unicode_100_ptr = value["unicode_100"].as_str().expect("string").as_ptr();

    compact_debug_json_strings(&mut value);

    assert_eq!(value["ascii_99"], serde_json::json!("a".repeat(99)));
    assert_eq!(value["ascii_100"], serde_json::json!("b".repeat(100)));
    assert_eq!(value["unicode_100"], serde_json::json!("é".repeat(50)));
    assert_eq!(
        value["ascii_99"].as_str().expect("string").as_ptr(),
        ascii_99_ptr
    );
    assert_eq!(
        value["ascii_100"].as_str().expect("string").as_ptr(),
        ascii_100_ptr
    );
    assert_eq!(
        value["unicode_100"].as_str().expect("string").as_ptr(),
        unicode_100_ptr
    );

    let expected = serde_json::json!({
        "ascii_99": "a".repeat(99),
        "ascii_100": "b".repeat(100),
        "ascii_101": format!("{}┄total 101┄{}", "c".repeat(20), "c".repeat(20)),
        "unicode_100": "é".repeat(50),
        "unicode_102": format!("{}┄total 102┄{}", "é".repeat(10), "é".repeat(10)),
        "unicode_split_edges": format!("{}┄total 102┄{}", "a".repeat(19), "c".repeat(19)),
    });
    assert_eq!(value, expected);
    assert_eq!(
        serde_json::to_vec(&value).expect("serialize compacted JSON"),
        serde_json::to_vec(&expected).expect("serialize expected JSON"),
        "long-string JSON bytes must retain the established compaction shape"
    );
}

/// Recurses through arrays and maps without replacing short values already
/// redacted by the debug projection.
#[test]
fn compact_debug_json_strings_recurses_without_replacing_redacted_values() {
    let mut value = serde_json::json!({
        "nested": [
            {
                "already_redacted": "<redacted>",
                "long": "x".repeat(101),
            },
            ["é".repeat(51)],
        ],
    });
    let redacted_ptr = value["nested"][0]["already_redacted"]
        .as_str()
        .expect("redacted string")
        .as_ptr();

    compact_debug_json_strings(&mut value);

    assert_eq!(
        value,
        serde_json::json!({
            "nested": [
                {
                    "already_redacted": "<redacted>",
                    "long": format!("{}┄total 101┄{}", "x".repeat(20), "x".repeat(20)),
                },
                [format!("{}┄total 102┄{}", "é".repeat(10), "é".repeat(10))],
            ],
        })
    );
    assert_eq!(
        value["nested"][0]["already_redacted"]
            .as_str()
            .expect("redacted string")
            .as_ptr(),
        redacted_ptr,
        "redaction precedes compaction and short redacted values stay in place"
    );
}

/// Full provider prompts become a bounded content-free summary before JSON
/// conversion, so neither text nor image bytes enter the debug mirror.
#[test]
fn full_prompt_debug_projection_is_fixed_shape_and_content_free() {
    let sentinel = b"\x89PNG\r\n\x1a\nunique-image-sentinel".to_vec();
    let tool_result = tau_proto::ToolResultItem {
        presentation: Default::default(),
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
                            tau_proto::OpaqueProviderItem::from_raw_json(
                                r#"{"type":"unique-raw-json-secret"}"#,
                            )
                            .expect("valid unknown provider item"),
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
        hosted_tools: Vec::new(),
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
    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor"),
        HarnessInputMessage::InterceptReply(tau_proto::InterceptReply {
            action: tau_proto::InterceptAction::Pass(Some(Box::new(event))),
        }),
    ))
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
        presentation: Default::default(),
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
    let message = HarnessInputMessage::InterceptReply(tau_proto::InterceptReply {
        action: tau_proto::InterceptAction::Pass(Some(Box::new(Event::ProviderToolResult(
            tau_proto::ToolResult {
                presentation: Default::default(),
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

    assert!(matches!(
        debug_harness_input_projection(&message),
        DebugHarnessInputProjection::Redacted(_)
    ));
    assert_eq!(
        compacted_json_bytes(debug_harness_input_json(&message)),
        compacted_json_bytes(legacy_debug_harness_input_json(&message)),
        "redacted binary input changed its bounded JSON projection"
    );

    let mut message = message;
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

    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        tau_proto::ConnectionId::parse("conn-1")
            .expect("test connection id must satisfy the identifier grammar"),
        HarnessInputMessage::emit(event),
    ))
    .expect("skip transient harness event");

    let lines = read_lines(log.path());
    assert!(
        lines.is_empty(),
        "transient streaming events are logged on publish; the raw inbound copy is redundant"
    );
}

/// Provider-controlled live retry detail must never cross into either
/// non-authoritative debug-log projection.
#[test]
fn retry_debug_projection_excludes_provider_detail_canary() {
    let secret = "PROVIDER_SECRET_CANARY";
    let event = Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: AgentPromptId::parse("sp-0").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: format!("retrying: {secret}"),
            clear_response: true,
            retry: Some(tau_proto::ProviderRetryStatus {
                category: tau_proto::ProviderRetryCategory::Overload,
                attempt: 2,
                next_retry_delay_secs: 13,
            }),
        }),
        response_stats: None,
        originator: PromptOriginator::User,
    });
    let Event::ProviderResponseUpdated(updated) = &event else {
        unreachable!();
    };
    let reported = debug_event_json(&Event::ProviderResponseUpdatedReported(updated.clone()));
    assert_eq!(reported["event"], "provider.response_updated_reported");
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open debug log");
    log.log_published_event(None, &event, UnixMicros::now())
        .expect("write published retry");
    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        crate::test_connection_id("provider"),
        HarnessInputMessage::emit_with_persist(event.clone(), true),
    ))
    .expect("write inbound retry");
    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        crate::test_connection_id("interceptor"),
        HarnessInputMessage::InterceptReply(tau_proto::InterceptReply {
            action: tau_proto::InterceptAction::Pass(Some(Box::new(event))),
        }),
    ))
    .expect("write intercepted retry");
    let lines = read_lines(log.path());
    // Transient response updates remain absent at the raw inbound boundary even
    // when a malformed peer asks to persist one; only the content-free published
    // and interceptor projections are logged.
    assert_eq!(lines.len(), 2);
    let expected = serde_json::json!({
        "event": "provider.response_updated",
        "payload": {
            "agent_prompt_id": "sp-0",
            "agent_id": "main",
            "status": {
                "text": "retrying",
                "clear_response": true,
                "retry": {
                    "category": "overload",
                    "attempt": 2,
                    "next_retry_delay_secs": 13,
                },
            },
            "originator": {"kind": "user"},
        },
    });
    let projected = [
        &lines[0]["event"],
        &lines[1]["event"]["payload"]["action"]["value"],
    ];
    for projection in projected {
        assert_eq!(projection, &expected);
    }
    for line in &lines {
        let encoded = line.to_string();
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("retrying:"));
    }
}

/// Dedicated UI debug-stat requests stay out of debug JSONL just as the
/// superseded transient request event did.
#[test]
fn ui_debug_event_stats_requests_are_not_logged() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");

    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        tau_proto::ConnectionId::parse("ui")
            .expect("test connection id must satisfy the identifier grammar"),
        HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: crate::test_extension_name("secret-extension"),
        }),
    ))
    .expect("skip UI debug stats request");

    assert!(read_lines(log.path()).is_empty());
}

/// Dedicated Provider capture messages never enter debug JSONL because even a
/// compacted projection would duplicate opaque sensitive artifact bytes.
#[test]
fn provider_debug_captures_are_not_logged() {
    let td = tempfile::tempdir().expect("tempdir");
    let mut log = DebugEventLog::open(td.path()).expect("open");
    log.log_harness_event(&HarnessEvent::from_connection_for_test(
        tau_proto::ConnectionId::parse("provider").expect("connection"),
        HarnessInputMessage::ProviderDebugCapture(tau_proto::ProviderDebugCapture {
            session_id: SessionId::parse("session").expect("session"),
            agent_prompt_id: AgentPromptId::parse("prompt").expect("prompt"),
            class: tau_proto::ProviderDebugCaptureClass::HttpSseRequest,
            zstd: b"opaque-sensitive-capture".to_vec(),
        }),
    ))
    .expect("skip Provider capture");
    assert!(read_lines(log.path()).is_empty());
}

/// Ensures raw terminal provider reports cannot leak embedded provider-image
/// bytes into debug JSONL.
#[test]
fn provider_finished_report_clears_image_bytes_before_debug_serialization() {
    let event = Event::ProviderResponseFinishedReported(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: AgentPromptId::parse("sp-image")
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![tau_proto::ContextItem::ToolResult(
            tau_proto::ToolResultItem {
                presentation: Default::default(),
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    });

    let message = HarnessInputMessage::emit(event.clone());
    assert!(matches!(
        debug_harness_input_projection(&message),
        DebugHarnessInputProjection::Redacted(_)
    ));
    assert_eq!(
        compacted_json_bytes(debug_harness_input_json(&message)),
        compacted_json_bytes(legacy_debug_harness_input_json(&message)),
        "provider terminal image redaction changed its bounded JSON projection"
    );

    let mut event = event;
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

/// A compacted replacement window with an image still takes the owned redaction
/// path and retains the former byte-exact bounded debug projection.
#[test]
fn compacted_window_image_debug_projection_matches_legacy_json() {
    let message = HarnessInputMessage::emit(Event::AgentCompacted(tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        compact_prompt_id: None,
        model: None,
        operation: None,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        transaction_id: None,
        cut: None,
        suffix_end: None,
        replacement_window: vec![tau_proto::ContextItem::ToolResult(
            tau_proto::ToolResultItem {
                presentation: Default::default(),
                call_id: "call-image".into(),
                tool_type: tau_proto::ToolType::Function,
                status: tau_proto::ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Null),
                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                    tau_proto::ImageContent {
                        media_type: tau_proto::ImageMediaType::Png,
                        data: b"compacted-window-image-sentinel".to_vec().into(),
                        width: 1,
                        height: 1,
                        detail: tau_proto::ImageDetail::High,
                    },
                )],
            },
        )],
    }));

    assert!(matches!(
        debug_harness_input_projection(&message),
        DebugHarnessInputProjection::Redacted(_)
    ));
    let json = debug_harness_input_json(&message);
    assert_eq!(
        compacted_json_bytes(json.clone()),
        compacted_json_bytes(legacy_debug_harness_input_json(&message)),
        "compacted-window image redaction changed its bounded JSON projection"
    );
    assert_eq!(
        json["payload"]["event"]["payload"]["replacement_window"][0]["payload"]["provider_content"]
            [0]["content"]["data"],
        serde_json::json!([])
    );
}
