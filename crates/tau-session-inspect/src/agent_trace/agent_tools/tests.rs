//! Focused helpers for compact explicit-observation projection.

use tau_proto::{AgentMessageKind, AgentMessageRecipient, ContentPart, ContextRole};

use super::*;

/// Wraps family records in deterministic common timing for serialization tests.
fn timed_records(records: Vec<Record>) -> Vec<TimedRecord> {
    records
        .into_iter()
        .enumerate()
        .map(|(index, record)| TimedRecord {
            common: RecordCommon {
                at_us: index as u64,
                recorded_at_unix_micros: Some(1_000 + index as u64),
                agent_id: AgentId::parse("agent-a").expect("agent"),
                journal_seq: index as u64,
                item_index: None,
            },
            record,
        })
        .collect()
}

fn id(byte: u8) -> ObservationId {
    ObservationId::from_bytes([byte; 16])
}

fn fact(agent: &str, byte: u8, seq: u64, at: u64, event: Event) -> Fact {
    Fact {
        agent_id: AgentId::parse(agent).expect("agent id"),
        id: id(byte),
        at: UnixMicros::new(at),
        seq: PersistedAgentEventSeq::new(seq),
        event,
    }
}

/// Equal timestamp/owner/sequence/item prefixes must use the exact approved
/// family order rather than declaration or insertion order.
#[test]
fn record_family_rank_uses_approved_equal_prefix_order() {
    let mut ranks = [
        RecordRank::Relationship,
        RecordRank::MessageReceived,
        RecordRank::Call,
        RecordRank::Activation,
        RecordRank::AssistantReasoning,
        RecordRank::MessageSent,
        RecordRank::AssistantMessage,
    ];
    ranks.sort_unstable();
    assert_eq!(
        ranks,
        [
            RecordRank::Call,
            RecordRank::AssistantMessage,
            RecordRank::AssistantReasoning,
            RecordRank::MessageSent,
            RecordRank::MessageReceived,
            RecordRank::Activation,
            RecordRank::Relationship,
        ]
    );
}

/// Semantic projection must preserve provider item order, directional message
/// identity, complete-text metrics, and the origin established by omitted
/// facts.
#[test]
fn semantic_items_share_global_journal_timing_and_order() {
    let mut provider = declaration("agent-a", 2, 1, "unused");
    provider.at = UnixMicros::new(10);
    let Event::ProviderResponseFinished(response) = &mut provider.event else {
        panic!("declaration helper must create a provider finish");
    };
    response.output_items = vec![
        ContextItem::Message(tau_proto::MessageItem {
            role: ContextRole::Assistant,
            content: vec![
                ContentPart::Text {
                    text: "hello ".into(),
                },
                ContentPart::Text {
                    text: "world".into(),
                },
            ],
            phase: Some(tau_proto::MessagePhase::Commentary),
            responses_raw_json: Some("must-not-project".into()),
        }),
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Summary,
            text: "because".into(),
        }),
        ContextItem::Reasoning(tau_proto::OpaqueProviderItem::new(CborValue::Text(
            "opaque".into(),
        ))),
    ];
    let sent = fact(
        "agent-a",
        3,
        2,
        20,
        Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("message-1").unwrap(),
            sender_id: AgentId::parse("agent-a").expect("agent"),
            recipient: AgentMessageRecipient::Agent {
                agent_id: AgentId::parse("agent-b").expect("agent"),
            },
            kind: AgentMessageKind::Message,
            message: "outbound".into(),
        }),
    );
    let received = fact(
        "agent-b",
        4,
        0,
        15,
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("message-1").unwrap(),
            sender_id: AgentId::parse("agent-a").expect("agent"),
            sender_session_id: None,
            recipient_id: AgentId::parse("agent-b").expect("agent"),
            kind: AgentMessageKind::Message,
            watch_turn_state: None,
            watch_provider_status: None,
            message: "outbound".into(),
        }),
    );
    let omitted_origin = fact("agent-a", 1, 0, 1, Event::TermBell(tau_proto::TermBell {}));

    let records = project_facts(
        vec![sent, received, provider, omitted_origin],
        super::super::AgentTraceMode::Lite,
    )
    .expect("semantic projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    assert_eq!(
        records
            .iter()
            .map(|record| record["record_type"].as_str().expect("record type"))
            .collect::<Vec<_>>(),
        [
            "assistant_message",
            "assistant_reasoning",
            "message_received",
            "message_sent",
        ]
    );
    assert_eq!(records[0]["at_us"], 9);
    assert_eq!(records[0]["recorded_at_unix_micros"], 10);
    assert_eq!(records[0]["journal_seq"], 1);
    assert_eq!(records[0]["item_index"], 0);
    assert_eq!(records[0]["text"], "hello world");
    assert_eq!(records[0]["text_bytes"], 11);
    assert_eq!(records[0]["text_lines"], 1);
    assert_eq!(records[0]["text_complete"], true);
    assert!(records[0].get("responses_raw_json").is_none());
    assert_eq!(records[1]["reasoning_kind"], "summary");
    assert_eq!(records[2]["message_id"], "message-1");
    assert_eq!(records[3]["recipient_kind"], "agent");
    assert_eq!(records[3]["recipient_id"], "agent-b");
}

fn assert_projection_error(facts: Vec<Fact>, expected: &str) {
    let error = match project_facts(facts, super::super::AgentTraceMode::Lite) {
        Ok(_) => panic!("projection must fail"),
        Err(error) => error,
    };
    match error {
        InspectError::Trace(crate::AgentTraceError::Projection(message)) => {
            assert!(
                message.contains(expected),
                "unexpected projection error: {message}"
            );
        }
        other => panic!("unexpected error variant: {other}"),
    }
}

fn declaration(agent: &str, byte: u8, seq: u64, call_id: &str) -> Fact {
    fact(
        agent,
        byte,
        seq,
        seq + 1,
        Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
            agent_prompt_id: format!("prompt-{call_id}")
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: AgentId::parse(agent).expect("agent id"),
            output_items: vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
                call_id: call_id.into(),
                name: tau_proto::ToolName::new("test"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Null,
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            estimated_api_cost_increment: None,
            estimated_api_cost_rates: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    )
}

fn call(declaration: u8) -> ToolCallRef {
    ToolCallRef {
        declaration: id(declaration),
        item_index: 0,
    }
}

fn tool_error(agent: &str, byte: u8, seq: u64, call_id: &str) -> Fact {
    fact(
        agent,
        byte,
        seq,
        seq + 1,
        Event::ProviderToolError(tau_proto::ToolError {
            call_id: call_id.into(),
            tool_name: tau_proto::ToolName::new("test"),
            tool_type: tau_proto::ToolType::Function,
            message: "failed".into(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    )
}

/// Lite output clipping preserves UTF-8 boundaries and reports incompleteness.
#[test]
fn lite_output_bounds_utf8() {
    let output = format!("{}érest", "a".repeat(LITE_OUTPUT_BYTES - 1));
    let (bounded, complete) = lite_output(&output);
    assert_eq!(bounded.len(), LITE_OUTPUT_BYTES - 1);
    assert!(!complete);
}

/// Floating-point arguments use the lossless tagged-CBOR representation.
#[test]
fn floats_are_not_coerced_to_json() {
    assert!(faithful_json(&CborValue::Float(-0.0)).is_none());
}

/// Non-JSON CBOR must retain duplicate keys, bytes, tags, and float bits rather
/// than being coerced into a lossy JSON value.
#[test]
fn non_json_arguments_keep_complete_tagged_cbor_structure() {
    let value = CborValue::Map(vec![
        (
            CborValue::Text("duplicate".into()),
            CborValue::Float(f64::NAN),
        ),
        (
            CborValue::Text("duplicate".into()),
            CborValue::Bytes(vec![0, 1, 2]),
        ),
        (
            CborValue::Integer(1.into()),
            CborValue::Tag(42, Box::new(CborValue::Text("tagged".into()))),
        ),
    ]);
    let projected = arguments(&value);
    assert_eq!(projected["type"], "map");
    assert_eq!(projected["value"].as_array().expect("entries").len(), 3);
    assert_eq!(projected["value"][0]["value"]["type"], "float64_bits");
    assert_eq!(projected["value"][1]["value"]["type"], "bytes");
    assert_eq!(projected["value"][2]["value"]["type"], "tag");
}

/// Projection plus both encoders preserve tagged arguments, lite clipping
/// counts, and full control-bearing multiline output.
#[test]
fn projected_payload_transformations_match_json_and_toon() {
    use base64::Engine as _;

    for (mode, output) in [
        (
            super::super::AgentTraceMode::Lite,
            format!("\u{1b}{}\nend", "a".repeat(LITE_OUTPUT_BYTES + 32)),
        ),
        (
            super::super::AgentTraceMode::Full,
            "first\nsecond\u{1b}".to_owned(),
        ),
    ] {
        let mut declared = declaration("agent-a", 1, 0, "source");
        if let Event::ProviderResponseFinished(response) = &mut declared.event
            && let ContextItem::ToolCall(call) = &mut response.output_items[0]
        {
            call.arguments = CborValue::Map(vec![(
                CborValue::Bytes(vec![0, 1]),
                CborValue::Tag(7, Box::new(CborValue::Float(-0.0))),
            )]);
        }
        let facts = vec![
            declared,
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(output.clone()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
        ];
        let records = project_facts(facts, mode).expect("projection");
        let json = serde_json::to_value(&records[0]).expect("JSON record");
        assert_eq!(json["arguments"]["type"], "map");
        if mode == super::super::AgentTraceMode::Lite {
            assert_eq!(json["output_bytes"], output.len() + 3);
            assert_eq!(json["output_lines"], 2);
            assert_eq!(json["output_complete"], false);
        } else {
            assert!(
                json["output"]
                    .as_str()
                    .expect("full output")
                    .contains("first\nsecond")
            );
            assert_eq!(json["output_complete"], true);
        }

        let agent_id = AgentId::parse("agent-a").expect("agent");
        let header = Header {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id: &agent_id,
            included_agent_ids: vec![&agent_id],
            content: match mode {
                super::super::AgentTraceMode::Lite => "lite",
                super::super::AgentTraceMode::Full => "full",
            },
            time_unit: "microseconds",
            absolute_time: "unix_epoch_microseconds_at_journal_append_invocation",
            timing_basis: "producer_wall_clock_at_observation",
            causality: "explicit_observation_refs_only",
        };
        let mut encoded = Vec::new();
        toon::write(&header, records, &mut encoded).expect("TOON");
        let decoded: serde_json::Value =
            serde_toon::from_str(std::str::from_utf8(&encoded).expect("UTF-8")).expect("TOON");
        let toon_call = &decoded["items"][0];
        let toon_output = if let Some(output) = toon_call["output_base64"].as_str() {
            base64::engine::general_purpose::STANDARD
                .decode(output)
                .expect("Base64")
        } else {
            toon_call["output"]
                .as_str()
                .expect("direct output")
                .as_bytes()
                .to_vec()
        };
        assert_eq!(
            toon_output,
            json["output"].as_str().expect("JSON output").as_bytes()
        );
        let toon_arguments = if let Some(arguments) = toon_call["arguments_json_base64"].as_str() {
            serde_json::from_slice::<serde_json::Value>(
                &base64::engine::general_purpose::STANDARD
                    .decode(arguments)
                    .expect("Base64"),
            )
            .expect("JSON arguments")
        } else {
            toon_call["arguments"].clone()
        };
        assert_eq!(toon_arguments, json["arguments"]);
    }
}

/// Structured shell outcomes must come from raw canonical CBOR, preserve
/// lifecycle status, and serialize identically in lite/full JSON and TOON.
#[test]
fn shell_outcome_preserves_lifecycle_and_encoding_parity() {
    for mode in [
        super::super::AgentTraceMode::Lite,
        super::super::AgentTraceMode::Full,
    ] {
        let mut declared = declaration("agent-a", 1, 0, "shell-call");
        let Event::ProviderResponseFinished(response) = &mut declared.event else {
            panic!("declaration helper must create a provider finish");
        };
        let ContextItem::ToolCall(declared_call) = &mut response.output_items[0] else {
            panic!("declaration helper must create a tool call");
        };
        declared_call.name = tau_proto::ToolName::new("shell_command");
        let facts = vec![
            declared,
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "shell-call".into(),
                    tool_name: tau_proto::ToolName::new("gpt_shell"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Map(vec![
                        (
                            CborValue::Text("output".into()),
                            CborValue::Text("status: 0".into()),
                        ),
                        (
                            CborValue::Text("status".into()),
                            CborValue::Integer(100.into()),
                        ),
                        (
                            CborValue::Text("termination_reason".into()),
                            CborValue::Text("exit".into()),
                        ),
                    ]),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
        ];
        let records = project_facts(facts, mode).expect("projection");
        let json = serde_json::to_value(&records[0]).expect("JSON record");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["shell_outcome"]["source"], "tool_result");
        assert_eq!(json["shell_outcome"]["success"], false);
        assert_eq!(json["shell_outcome"]["termination_reason"], "exit");
        assert_eq!(json["shell_outcome"]["exit_code"], 100);
        assert!(json["shell_outcome"].get("timed_out").is_none());

        let agent_id = AgentId::parse("agent-a").expect("agent");
        let header = Header {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id: &agent_id,
            included_agent_ids: vec![&agent_id],
            content: mode.label(),
            time_unit: "microseconds",
            absolute_time: "unix_epoch_microseconds_at_journal_append_invocation",
            timing_basis: "producer_wall_clock_at_observation",
            causality: "explicit_observation_refs_only",
        };
        let mut encoded = Vec::new();
        toon::write(&header, records, &mut encoded).expect("TOON");
        let toon: serde_json::Value =
            serde_toon::from_str(std::str::from_utf8(&encoded).expect("UTF-8")).expect("TOON");
        assert_eq!(toon["items"][0]["shell_outcome"], json["shell_outcome"]);
    }
}

/// Cancellation classification must suppress structured result details even
/// when a same-call canonical terminal carries a coherent shell map.
#[test]
fn cancellation_never_projects_a_shell_outcome() {
    let mut source = declaration("agent-a", 1, 0, "source");
    let Event::ProviderResponseFinished(response) = &mut source.event else {
        panic!("declaration helper must create a provider finish");
    };
    let ContextItem::ToolCall(call_item) = &mut response.output_items[0] else {
        panic!("declaration helper must create a tool call");
    };
    call_item.name = tau_proto::ToolName::new("shell");
    let records = project_facts(
        vec![
            source,
            declaration("agent-a", 2, 1, "cancel"),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolCancellationRequested(tau_proto::AgentToolCancellationRequested {
                    cancel_call: call(2),
                    target_call: call(1),
                }),
            ),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(5),
                    cause: tau_proto::ToolTerminalCause::Cancellation { request: id(3) },
                }),
            ),
            fact(
                "agent-a",
                5,
                4,
                5,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Map(vec![(
                        CborValue::Text("status".into()),
                        CborValue::Integer(0.into()),
                    )]),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("cancelled projection");
    let source = serde_json::to_value(&records[0]).expect("JSON");
    assert_eq!(source["status"], "cancelled");
    assert!(source.get("shell_outcome").is_none());
}

/// Lookalike structured fields from a non-shell declaration must not acquire
/// shell-specific semantics.
#[test]
fn non_shell_calls_omit_shell_outcomes() {
    let records = project_facts(
        vec![
            declaration("agent-a", 1, 0, "source"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Map(vec![(
                        CborValue::Text("status".into()),
                        CborValue::Integer(0.into()),
                    )]),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("non-shell projection");
    let call = serde_json::to_value(&records[0]).expect("JSON");
    assert_eq!(call["status"], "ok");
    assert!(call.get("shell_outcome").is_none());
}

/// Completion delivery must leave payload ownership on the source terminal in
/// both output modes and expose only an explicit wait reference.
#[test]
fn completion_wait_references_source_owned_output_in_lite_and_full() {
    for mode in [
        super::super::AgentTraceMode::Lite,
        super::super::AgentTraceMode::Full,
    ] {
        let facts = vec![
            declaration("agent-a", 1, 0, "source"),
            declaration("agent-a", 2, 1, "wait"),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(4),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("source-output".into()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            fact(
                "agent-a",
                5,
                4,
                5,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(2),
                    terminal: id(6),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                6,
                5,
                6,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "wait".into(),
                    tool_name: tau_proto::ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("copied-output-must-not-survive".into()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            fact(
                "agent-a",
                98,
                6,
                7,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(2),
                    mode: tau_proto::ToolWaitMode::Exact { target: call(1) },
                }),
            ),
            fact(
                "agent-a",
                7,
                7,
                8,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(98),
                    wait_call: call(2),
                    registration: None,
                    wait_terminal: id(6),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(1),
                        source_terminal: id(4),
                        source_phase: tau_proto::ToolSourcePhase::Foreground,
                        envelope: tau_proto::ToolOutputEnvelope::Identity,
                    },
                }),
            ),
        ];
        let records = project_facts(facts, mode)
            .expect("projection")
            .into_iter()
            .map(|record| serde_json::to_value(record).expect("record JSON"))
            .collect::<Vec<_>>();
        let source = records
            .iter()
            .find(|r| r["call_id"] == "source")
            .expect("source");
        let wait = records
            .iter()
            .find(|r| r["call_id"] == "wait")
            .expect("wait");
        let settlement = records
            .iter()
            .find(|r| r["relationship"] == "wait_settlement")
            .expect("settlement");
        assert_eq!(source["output"], "source-output");
        assert!(wait.get("output").is_none());
        assert!(wait.get("output_bytes").is_none());
        assert_eq!(settlement["output_ref"], id(4).to_string());
    }
}

/// A completion settlement remains sufficient negative evidence that the wait
/// terminal copied source output even when observation/registration evidence is
/// unavailable.
#[test]
fn incomplete_completion_wait_never_duplicates_source_output() {
    for missing_registration in [false, true] {
        let mut facts = vec![
            declaration("agent-a", 1, 0, "source"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("source-owned".into()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            declaration("agent-a", 4, 3, "wait"),
            fact(
                "agent-a",
                5,
                4,
                5,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(4),
                    terminal: id(6),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                6,
                5,
                6,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "wait".into(),
                    tool_name: tau_proto::ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("copied".into()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
        ];
        if missing_registration {
            facts.push(fact(
                "agent-a",
                8,
                6,
                8,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(4),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ));
        }
        facts.push(fact(
            "agent-a",
            7,
            7,
            9,
            Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                wait_observation: if missing_registration { id(8) } else { id(98) },
                wait_call: call(4),
                registration: missing_registration.then(|| id(99)),
                wait_terminal: id(6),
                outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                    source_call: call(1),
                    source_terminal: id(3),
                    source_phase: tau_proto::ToolSourcePhase::Background,
                    envelope: tau_proto::ToolOutputEnvelope::Identity,
                },
            }),
        ));

        let records = project_facts(facts, super::super::AgentTraceMode::Full)
            .expect("incomplete wait projection")
            .into_iter()
            .map(|record| serde_json::to_value(record).expect("record JSON"))
            .collect::<Vec<_>>();
        let source = records
            .iter()
            .find(|record| record["call_id"] == "source")
            .expect("source");
        let wait = records
            .iter()
            .find(|record| record["call_id"] == "wait")
            .expect("wait");
        let settlement = records
            .iter()
            .find(|record| record["relationship"] == "wait_settlement")
            .expect("settlement");
        assert_eq!(source["output"], "source-owned");
        assert_eq!(wait["status"], "incomplete");
        assert!(wait.get("output").is_none());
        assert_eq!(settlement["source_resolution"], "source_not_selected");
        assert!(settlement.get("completion_to_delivery_us").is_none());
    }
}

/// Exact-target locality and mode agreement are transitive through wait
/// observation, registration, and completion settlement references.
#[test]
fn exact_wait_transitive_consistency_is_enforced() {
    let fallback = project_facts(
        vec![
            declaration("agent-a", 1, 0, "source"),
            declaration("agent-a", 2, 1, "wait"),
            declaration("agent-b", 20, 0, "foreign-target"),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(2),
                    mode: tau_proto::ToolWaitMode::Exact { target: call(20) },
                }),
            ),
            tool_error("agent-a", 4, 3, "source"),
            tool_error("agent-a", 5, 4, "wait"),
            fact(
                "agent-a",
                6,
                5,
                6,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(3),
                    wait_call: call(2),
                    registration: None,
                    wait_terminal: id(5),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(1),
                        source_terminal: id(4),
                        source_phase: tau_proto::ToolSourcePhase::Foreground,
                        envelope: tau_proto::ToolOutputEnvelope::Identity,
                    },
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("foreign exact target fallback")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();
    let observation = fallback
        .iter()
        .find(|record| record["relationship"] == "wait_observation")
        .expect("observation");
    assert_eq!(observation["mode"], "exact_unresolved");
    let settlement = fallback
        .iter()
        .find(|record| record["relationship"] == "wait_settlement")
        .expect("settlement");
    assert_eq!(settlement["source_resolution"], "source_not_selected");
    assert!(settlement.get("completion_to_delivery_us").is_none());

    let disagreement = vec![
        declaration("agent-a", 1, 0, "source"),
        declaration("agent-a", 2, 1, "wait"),
        fact(
            "agent-a",
            3,
            2,
            3,
            Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                wait_call: call(2),
                mode: tau_proto::ToolWaitMode::Exact { target: call(1) },
            }),
        ),
        fact(
            "agent-a",
            4,
            3,
            4,
            Event::AgentToolWaitRegistered(tau_proto::AgentToolWaitRegistered {
                wait_observation: id(3),
                wait_call: call(2),
                mode: tau_proto::ToolWaitMode::NextBackground,
            }),
        ),
        fact(
            "agent-a",
            5,
            4,
            5,
            Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                wait_observation: id(3),
                wait_call: call(2),
                registration: Some(id(4)),
                wait_terminal: id(99),
                outcome: tau_proto::ToolWaitOutcome::TimedOut,
            }),
        ),
    ];
    assert_projection_error(disagreement, "contradicts wait call");

    let wrong_source = vec![
        declaration("agent-a", 1, 0, "source"),
        declaration("agent-a", 2, 1, "other"),
        declaration("agent-a", 3, 2, "wait"),
        fact(
            "agent-a",
            4,
            3,
            4,
            Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                wait_call: call(3),
                mode: tau_proto::ToolWaitMode::Exact { target: call(1) },
            }),
        ),
        fact(
            "agent-a",
            5,
            4,
            5,
            Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                wait_observation: id(4),
                wait_call: call(3),
                registration: None,
                wait_terminal: id(99),
                outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                    source_call: call(2),
                    source_terminal: id(98),
                    source_phase: tau_proto::ToolSourcePhase::Foreground,
                    envelope: tau_proto::ToolOutputEnvelope::Identity,
                },
            }),
        ),
        fact(
            "agent-a",
            98,
            5,
            6,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "other".into(),
                tool_name: tau_proto::ToolName::new("test"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("other".into()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        ),
        fact(
            "agent-a",
            99,
            6,
            7,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "wait".into(),
                tool_name: tau_proto::ToolName::new("wait"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("done".into()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        ),
    ];
    assert_projection_error(wrong_source, "different selected source call");
}

/// Selected-local wrong-family wait references are integrity errors, while only
/// absent or foreign references qualify for fallback.
#[test]
fn local_wrong_family_wait_references_are_rejected() {
    let activation = |byte, seq| {
        fact(
            "agent-a",
            byte,
            seq,
            seq + 1,
            Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                kind: tau_proto::ActivationKind::Other,
                source_observation: None,
                source_call: None,
            }),
        )
    };
    assert_projection_error(
        vec![
            declaration("agent-a", 1, 0, "wait"),
            activation(2, 1),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolWaitRegistered(tau_proto::AgentToolWaitRegistered {
                    wait_observation: id(2),
                    wait_call: call(1),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
        ],
        "contradicts wait call",
    );

    assert_projection_error(
        vec![
            declaration("agent-a", 1, 0, "wait"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(1),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
            activation(3, 2),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(2),
                    wait_call: call(1),
                    registration: Some(id(3)),
                    wait_terminal: id(5),
                    outcome: tau_proto::ToolWaitOutcome::Cancelled,
                }),
            ),
            tool_error("agent-a", 5, 4, "wait"),
        ],
        "contradictory registration",
    );
}

/// Queue-before-register activation outcomes are valid selected-local immediate
/// settlements for exact, bare, and activating-input waits.
#[test]
fn immediate_activation_wait_outcomes_project_without_registration() {
    for (mode, outcome) in [
        (
            tau_proto::ToolWaitMode::Exact { target: call(1) },
            tau_proto::ToolWaitOutcome::InterruptedByActivation { activation: id(4) },
        ),
        (
            tau_proto::ToolWaitMode::NextBackground,
            tau_proto::ToolWaitOutcome::InterruptedByActivation { activation: id(4) },
        ),
        (
            tau_proto::ToolWaitMode::ActivatingInput {
                effective_timeout_minutes: 1,
            },
            tau_proto::ToolWaitOutcome::InputAvailable { activation: id(4) },
        ),
    ] {
        let records = project_facts(
            vec![
                declaration("agent-a", 1, 0, "source"),
                declaration("agent-a", 2, 1, "wait"),
                fact(
                    "agent-a",
                    3,
                    2,
                    3,
                    Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                        wait_call: call(2),
                        mode,
                    }),
                ),
                fact(
                    "agent-a",
                    4,
                    3,
                    4,
                    Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                        kind: tau_proto::ActivationKind::VisibleUser,
                        source_observation: None,
                        source_call: None,
                    }),
                ),
                tool_error("agent-a", 5, 4, "wait"),
                fact(
                    "agent-a",
                    6,
                    5,
                    6,
                    Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                        wait_observation: id(3),
                        wait_call: call(2),
                        registration: None,
                        wait_terminal: id(5),
                        outcome,
                    }),
                ),
            ],
            super::super::AgentTraceMode::Lite,
        )
        .expect("immediate activation settlement");
        let settlement = records
            .into_iter()
            .map(|record| serde_json::to_value(record).expect("record JSON"))
            .find(|record| record["relationship"] == "wait_settlement")
            .expect("settlement");
        assert_eq!(settlement["registration"], "immediate");
        assert!(settlement["registration_ref"].is_null());
        assert_eq!(settlement["source_resolution"], "resolved");
    }
}

/// A crash-tail or selected-cut loss must remain visibly incomplete instead of
/// reconstructing terminal or settlement relationships.
#[test]
fn crash_tail_and_selected_cut_remain_explicitly_incomplete() {
    let records = project_facts(
        vec![
            declaration("agent-a", 1, 0, "wait"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolWaitRegistered(tau_proto::AgentToolWaitRegistered {
                    wait_observation: id(99),
                    wait_call: call(1),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(99),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("partial projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();
    let registration = records
        .iter()
        .find(|r| r["relationship"] == "wait_registration")
        .expect("registration");
    let call = records
        .iter()
        .find(|r| r["record_type"] == "call")
        .expect("call");
    assert_eq!(registration["outcome"], "incomplete");
    assert_eq!(call["status"], "incomplete");
    assert_eq!(call["terminal"], id(99).to_string());
    assert_eq!(call["terminal_resolution"], "source_not_selected");
}

/// A foreign background completion remains non-fatal without exposing a new
/// cross-journal schema state or attributing its output to another agent.
#[test]
fn foreign_background_completion_uses_unresolved_fallbacks() {
    let records = project_facts(
        vec![
            declaration("agent-b", 1, 0, "source"),
            fact(
                "agent-b",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-b",
                3,
                2,
                3,
                Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("child-owned".into()),
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            declaration("agent-a", 4, 0, "wait"),
            fact(
                "agent-a",
                5,
                1,
                5,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(4),
                    mode: tau_proto::ToolWaitMode::Exact { target: call(1) },
                }),
            ),
            fact(
                "agent-a",
                6,
                2,
                6,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(4),
                    terminal: id(7),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                7,
                3,
                7,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "wait".into(),
                    tool_name: tau_proto::ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text("delivered".into()),
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            fact(
                "agent-a",
                8,
                4,
                8,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(5),
                    wait_call: call(4),
                    registration: None,
                    wait_terminal: id(7),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(1),
                        source_terminal: id(3),
                        source_phase: tau_proto::ToolSourcePhase::Background,
                        envelope: tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
                    },
                }),
            ),
            fact(
                "agent-a",
                9,
                5,
                9,
                Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                    kind: tau_proto::ActivationKind::BackgroundCompletion,
                    source_observation: Some(id(3)),
                    source_call: Some(call(1)),
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("foreign-endpoint projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    let observation = records
        .iter()
        .find(|record| record["relationship"] == "wait_observation")
        .expect("wait observation");
    assert_eq!(observation["mode"], "exact_unresolved");

    let settlement = records
        .iter()
        .find(|record| record["relationship"] == "wait_settlement")
        .expect("wait settlement");
    assert_eq!(settlement["source_resolution"], "source_not_selected");
    assert!(settlement.get("completion_to_delivery_us").is_none());

    let activation = records
        .iter()
        .find(|record| record["record_type"] == "activation")
        .expect("activation");
    assert_eq!(activation["source_resolution"], "source_not_selected");
    assert!(
        activation
            .get("completion_to_activation_queue_us")
            .is_none()
    );

    let source = records
        .iter()
        .find(|record| record["call_id"] == "source")
        .expect("source call");
    assert_eq!(source["output"], "child-owned");
    assert!(
        !records
            .iter()
            .any(|record| record["agent_id"] == "agent-a" && record["output"] == "child-owned")
    );
}

/// A foreign terminal cannot transfer status or output to a declaration in
/// another agent journal.
#[test]
fn foreign_terminal_does_not_transfer_call_status_or_output() {
    let records = project_facts(
        vec![
            declaration("agent-b", 1, 0, "source"),
            declaration("agent-a", 2, 0, "cancel"),
            fact(
                "agent-a",
                3,
                1,
                3,
                Event::AgentToolCancellationRequested(tau_proto::AgentToolCancellationRequested {
                    cancel_call: call(2),
                    target_call: call(1),
                }),
            ),
            fact(
                "agent-a",
                4,
                2,
                4,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(5),
                    cause: tau_proto::ToolTerminalCause::Cancellation { request: id(3) },
                }),
            ),
            fact(
                "agent-a",
                5,
                3,
                5,
                Event::ToolCancelled(tau_proto::ToolCancelled {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                }),
            ),
        ],
        super::super::AgentTraceMode::Full,
    )
    .expect("foreign terminal projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    let source = records
        .iter()
        .find(|record| record["call_id"] == "source")
        .expect("source call");
    assert_eq!(source["status"], "incomplete");
    assert!(source.get("terminal").is_none());
    assert!(source.get("terminal_resolution").is_none());
    assert!(source.get("output").is_none());
    assert!(source.get("dispatch_to_terminal_us").is_none());
    assert!(
        records
            .iter()
            .any(|record| record["relationship"] == "cancellation_requested")
    );
}

/// A classification whose canonical append was lost is superseded by the one
/// classification whose canonical terminal later commits.
#[test]
fn committed_terminal_supersedes_orphan_classifications() {
    for (winner, expected_status) in [
        (tau_proto::ToolTerminalCause::RestartRepair, "incomplete"),
        (tau_proto::ToolTerminalCause::ToolError, "error"),
    ] {
        let records = project_facts(
            vec![
                declaration("agent-a", 1, 0, "source"),
                fact(
                    "agent-a",
                    2,
                    1,
                    2,
                    Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                        call: call(1),
                        terminal: id(99),
                        cause: tau_proto::ToolTerminalCause::Completed,
                    }),
                ),
                fact(
                    "agent-a",
                    3,
                    2,
                    3,
                    Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                        call: call(1),
                        terminal: id(4),
                        cause: winner,
                    }),
                ),
                tool_error("agent-a", 4, 3, "source"),
            ],
            super::super::AgentTraceMode::Lite,
        )
        .expect("winner projection")
        .into_iter()
        .map(|record| serde_json::to_value(record).expect("record JSON"))
        .collect::<Vec<_>>();
        let source = records
            .iter()
            .find(|record| record["call_id"] == "source")
            .expect("source call");
        assert_eq!(source["terminal"], id(4).to_string());
        assert_eq!(source["status"], expected_status);
    }
}

/// Two classifications whose canonical terminals both committed remain a trace
/// integrity error rather than silently selecting by order.
#[test]
fn multiple_committed_terminal_classifications_are_rejected() {
    assert_projection_error(
        vec![
            declaration("agent-a", 1, 0, "source"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::ToolError,
                }),
            ),
            tool_error("agent-a", 3, 2, "source"),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(5),
                    cause: tau_proto::ToolTerminalCause::RestartRepair,
                }),
            ),
            tool_error("agent-a", 5, 4, "source"),
        ],
        "multiple committed terminal classifications",
    );
}

/// A foreign cancellation request makes the whole classification unavailable;
/// it neither exports cancelled semantics alone nor collides with a local
/// winner.
#[test]
fn foreign_cancellation_request_does_not_classify_local_terminal() {
    for include_winner in [false, true] {
        let mut facts = vec![
            declaration("agent-a", 1, 0, "source"),
            declaration("agent-b", 20, 0, "foreign-request"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(3),
                    cause: tau_proto::ToolTerminalCause::Cancellation { request: id(20) },
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::ToolCancelled(tau_proto::ToolCancelled {
                    call_id: "source".into(),
                    tool_name: tau_proto::ToolName::new("test"),
                    tool_type: tau_proto::ToolType::Function,
                }),
            ),
        ];
        if include_winner {
            facts.extend([
                fact(
                    "agent-a",
                    4,
                    3,
                    4,
                    Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                        call: call(1),
                        terminal: id(5),
                        cause: tau_proto::ToolTerminalCause::ToolError,
                    }),
                ),
                tool_error("agent-a", 5, 4, "source"),
            ]);
        }
        let records = project_facts(facts, super::super::AgentTraceMode::Lite)
            .expect("foreign request projection")
            .into_iter()
            .map(|record| serde_json::to_value(record).expect("record JSON"))
            .collect::<Vec<_>>();
        let source = records
            .iter()
            .find(|record| record["call_id"] == "source")
            .expect("source");
        if include_winner {
            assert_eq!(source["status"], "error");
            assert_eq!(source["terminal"], id(5).to_string());
        } else {
            assert_eq!(source["status"], "incomplete");
            assert_eq!(source["terminal"], id(3).to_string());
            assert_eq!(source["terminal_resolution"], "source_not_selected");
        }
    }
}

/// Selected endpoints in another journal are unavailable to local integrity
/// checks even when their event family would contradict a same-journal edge.
#[test]
fn wrong_journal_endpoint_families_degrade_without_projection_failure() {
    let records = project_facts(
        vec![
            declaration("agent-a", 1, 0, "wait-completion"),
            declaration("agent-a", 2, 1, "wait-activation"),
            declaration("agent-a", 3, 2, "cancelled"),
            declaration("agent-b", 20, 0, "foreign"),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(20),
                    wait_call: call(1),
                    registration: Some(id(20)),
                    wait_terminal: id(20),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(3),
                        source_terminal: id(20),
                        source_phase: tau_proto::ToolSourcePhase::Background,
                        envelope: tau_proto::ToolOutputEnvelope::Identity,
                    },
                }),
            ),
            fact(
                "agent-a",
                5,
                4,
                5,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(20),
                    wait_call: call(2),
                    registration: None,
                    wait_terminal: id(20),
                    outcome: tau_proto::ToolWaitOutcome::InterruptedByActivation {
                        activation: id(20),
                    },
                }),
            ),
            fact(
                "agent-a",
                6,
                5,
                6,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(3),
                    terminal: id(20),
                    cause: tau_proto::ToolTerminalCause::Cancellation { request: id(20) },
                }),
            ),
            fact(
                "agent-a",
                7,
                6,
                7,
                Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                    kind: tau_proto::ActivationKind::BackgroundCompletion,
                    source_observation: Some(id(20)),
                    source_call: Some(call(3)),
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("wrong-journal refs remain non-fatal")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    let completion = records
        .iter()
        .find(|record| record["outcome"] == "completion_delivered")
        .expect("completion settlement");
    assert_eq!(completion["registration"], "unresolved");
    assert_eq!(
        completion["wait_terminal_resolution"],
        "source_not_selected"
    );
    assert_eq!(completion["source_resolution"], "source_not_selected");
    assert!(completion.get("completion_to_delivery_us").is_none());

    let activation = records
        .iter()
        .find(|record| record["outcome"] == "interrupted_by_activation")
        .expect("activation settlement");
    assert_eq!(activation["source_resolution"], "source_not_selected");
    assert!(activation.get("activation_to_wait_terminal_us").is_none());

    let cancelled = records
        .iter()
        .find(|record| record["call_id"] == "cancelled")
        .expect("cancelled call");
    assert_eq!(cancelled["status"], "incomplete");
    assert_eq!(cancelled["terminal"], id(20).to_string());
    assert_eq!(cancelled["terminal_resolution"], "source_not_selected");
}

/// Local observation endpoints cannot lend timing, terminal, or output
/// semantics to a call declared in another journal.
#[test]
fn local_observations_with_foreign_calls_degrade_as_one_relationship() {
    let records = project_facts(
        vec![
            declaration("agent-b", 20, 0, "foreign"),
            fact(
                "agent-a",
                30,
                0,
                30,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(20),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
            fact(
                "agent-a",
                31,
                1,
                31,
                Event::AgentToolWaitRegistered(tau_proto::AgentToolWaitRegistered {
                    wait_observation: id(30),
                    wait_call: call(20),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
            tool_error("agent-a", 32, 2, "foreign"),
            fact(
                "agent-a",
                33,
                3,
                33,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(30),
                    wait_call: call(20),
                    registration: Some(id(31)),
                    wait_terminal: id(32),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(20),
                        source_terminal: id(32),
                        source_phase: tau_proto::ToolSourcePhase::Background,
                        envelope: tau_proto::ToolOutputEnvelope::Identity,
                    },
                }),
            ),
            fact(
                "agent-a",
                34,
                4,
                34,
                Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                    kind: tau_proto::ActivationKind::BackgroundCompletion,
                    source_observation: Some(id(32)),
                    source_call: Some(call(20)),
                }),
            ),
            fact(
                "agent-a",
                35,
                5,
                35,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(20),
                    terminal: id(32),
                    cause: tau_proto::ToolTerminalCause::ToolError,
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("mixed-locality projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    let registration = records
        .iter()
        .find(|record| record["relationship"] == "wait_registration")
        .expect("registration");
    assert_eq!(registration["outcome"], "incomplete");

    let settlement = records
        .iter()
        .find(|record| record["relationship"] == "wait_settlement")
        .expect("settlement");
    assert_eq!(settlement["registration"], "unresolved");
    assert_eq!(
        settlement["wait_terminal_resolution"],
        "source_not_selected"
    );
    assert_eq!(settlement["source_resolution"], "source_not_selected");
    assert!(settlement.get("active_wait_us").is_none());
    assert!(settlement.get("completion_to_delivery_us").is_none());

    let activation = records
        .iter()
        .find(|record| record["record_type"] == "activation")
        .expect("activation");
    assert_eq!(activation["source_resolution"], "source_not_selected");
    assert!(
        activation
            .get("completion_to_activation_queue_us")
            .is_none()
    );

    let foreign = records
        .iter()
        .find(|record| record["call_id"] == "foreign")
        .expect("foreign call");
    assert_eq!(foreign["status"], "incomplete");
    assert!(foreign.get("terminal").is_none());
    assert!(foreign.get("output").is_none());
}

/// Missing endpoint halves use the same atomic fallback as foreign halves and
/// cannot lend timing or call-index semantics to their selected counterpart.
#[test]
fn unavailable_endpoint_halves_do_not_resolve_partial_relationships() {
    let records = project_facts(
        vec![
            declaration("agent-a", 1, 0, "wait-source"),
            fact(
                "agent-a",
                2,
                1,
                2,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call: call(1),
                    mode: tau_proto::ToolWaitMode::NextBackground,
                }),
            ),
            fact(
                "agent-a",
                3,
                2,
                3,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(1),
                    terminal: id(4),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                4,
                3,
                4,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "wait-source".into(),
                    tool_name: tau_proto::ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Null,
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            tool_error("agent-a", 5, 4, "missing-source"),
            fact(
                "agent-a",
                6,
                5,
                6,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(2),
                    wait_call: call(1),
                    registration: None,
                    wait_terminal: id(4),
                    outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                        source_call: call(99),
                        source_terminal: id(5),
                        source_phase: tau_proto::ToolSourcePhase::Foreground,
                        envelope: tau_proto::ToolOutputEnvelope::Identity,
                    },
                }),
            ),
            declaration("agent-a", 10, 6, "wait-missing-observation"),
            fact(
                "agent-a",
                11,
                7,
                11,
                Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                    call: call(10),
                    terminal: id(12),
                    cause: tau_proto::ToolTerminalCause::Completed,
                }),
            ),
            fact(
                "agent-a",
                12,
                8,
                12,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "wait-missing-observation".into(),
                    tool_name: tau_proto::ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Null,
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::Final,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            ),
            fact(
                "agent-a",
                13,
                9,
                13,
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: id(98),
                    wait_call: call(10),
                    registration: None,
                    wait_terminal: id(12),
                    outcome: tau_proto::ToolWaitOutcome::TimedOut,
                }),
            ),
        ],
        super::super::AgentTraceMode::Lite,
    )
    .expect("unavailable halves projection")
    .into_iter()
    .map(|record| serde_json::to_value(record).expect("record JSON"))
    .collect::<Vec<_>>();

    let completion = records
        .iter()
        .find(|record| record["outcome"] == "completion_delivered")
        .expect("completion");
    assert_eq!(completion["source_resolution"], "source_not_selected");
    assert_eq!(
        completion["wait_terminal_resolution"],
        "source_not_selected"
    );
    assert!(completion.get("completion_to_delivery_us").is_none());

    let missing_observation_call = records
        .iter()
        .find(|record| record["call_id"] == "wait-missing-observation")
        .expect("call");
    assert_eq!(missing_observation_call["status"], "ok");
}

/// TOON Base64-frames control-bearing payloads instead of emitting terminal
/// controls raw or corrupting their exact JSON semantics.
#[test]
fn toon_frames_control_bearing_payload_fields() {
    use base64::Engine as _;

    let root_agent_id = AgentId::parse("agent-a").expect("agent");
    let header = Header {
        schema: SCHEMA,
        schema_version: 0,
        record_type: "header",
        root_agent_id: &root_agent_id,
        included_agent_ids: vec![],
        content: "full",
        time_unit: "microseconds",
        absolute_time: "unix_epoch_microseconds_at_journal_append_invocation",
        timing_basis: "producer_wall_clock_at_observation",
        causality: "explicit_observation_refs_only",
    };
    let arguments = serde_json::json!({"nested": "arg\u{1b}[31m"});
    let semantic = semantic::project_message_event(
        &Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("message\u{1b}").unwrap(),
            sender_id: root_agent_id.clone(),
            recipient: AgentMessageRecipient::User,
            kind: AgentMessageKind::Message,
            message: "secret\u{7}".into(),
        }),
        super::super::AgentTraceMode::Full,
    )
    .expect("semantic message")
    .record;
    let records = vec![
        Record::Call(CallRecord {
            record_type: "call",
            call: call(1),
            call_id: "call\u{1b}".into(),
            tool: tau_proto::ToolName::new("test"),
            command: Some("printf \u{7}".into()),
            arguments: arguments.clone(),
            declaration_to_dispatch_us: None,
            dispatch_to_backgrounded_us: None,
            lifecycle: CallLifecycleRecord::Resolved {
                status: CallStatus::Ok,
                terminal: id(2),
                cause: tau_proto::ToolTerminalCause::Completed,
                terminal_resolution: LocalResolution::Resolved,
                dispatch_to_terminal_us: None,
                backgrounded_to_terminal_us: None,
                shell_outcome: None,
                output: CallOutputRecord::Full {
                    output: "result\u{1b}[0m".into(),
                    output_complete: CompleteOutput,
                },
            },
        }),
        Record::Call(CallRecord {
            record_type: "call",
            call: call(3),
            call_id: "lite-control".into(),
            tool: tau_proto::ToolName::new("test"),
            command: None,
            arguments: serde_json::json!({}),
            declaration_to_dispatch_us: None,
            dispatch_to_backgrounded_us: None,
            lifecycle: CallLifecycleRecord::Resolved {
                status: CallStatus::Ok,
                terminal: id(4),
                cause: tau_proto::ToolTerminalCause::Completed,
                terminal_resolution: LocalResolution::Resolved,
                dispatch_to_terminal_us: None,
                backgrounded_to_terminal_us: None,
                shell_outcome: None,
                output: CallOutputRecord::Lite {
                    output_bytes: 12,
                    output_lines: 2,
                    output: "line1\nline2\u{1b}".into(),
                    output_complete: false,
                },
            },
        }),
        Record::Call(CallRecord {
            record_type: "call",
            call: call(5),
            call_id: "full-multiline".into(),
            tool: tau_proto::ToolName::new("test"),
            command: None,
            arguments: serde_json::json!({}),
            declaration_to_dispatch_us: None,
            dispatch_to_backgrounded_us: None,
            lifecycle: CallLifecycleRecord::Resolved {
                status: CallStatus::Ok,
                terminal: id(6),
                cause: tau_proto::ToolTerminalCause::Completed,
                terminal_resolution: LocalResolution::Resolved,
                dispatch_to_terminal_us: None,
                backgrounded_to_terminal_us: None,
                shell_outcome: None,
                output: CallOutputRecord::Full {
                    output: "line1\nline2".into(),
                    output_complete: CompleteOutput,
                },
            },
        }),
        Record::Semantic(semantic),
    ];

    let records = timed_records(records);
    let mut encoded = Vec::new();
    toon::write(&header, records, &mut encoded).expect("TOON");
    assert!(!encoded.contains(&0x1b));
    assert!(!encoded.contains(&0x07));
    let decoded: serde_json::Value =
        serde_toon::from_str(std::str::from_utf8(&encoded).expect("UTF-8")).expect("strict TOON");
    let record = &decoded["items"][0];
    for (field, expected) in [
        ("call_id_base64", b"call\x1b".as_slice()),
        ("command_base64", b"printf \x07".as_slice()),
        ("output_base64", b"result\x1b[0m".as_slice()),
    ] {
        let framed = record[field].as_str().expect("Base64 field");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(framed)
                .expect("Base64"),
            expected
        );
    }
    let framed_arguments = record["arguments_json_base64"]
        .as_str()
        .expect("Base64 arguments");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &base64::engine::general_purpose::STANDARD
                .decode(framed_arguments)
                .expect("Base64")
        )
        .expect("JSON"),
        arguments
    );
    let lite = &decoded["items"][1];
    assert_eq!(lite["output_bytes"], 12);
    assert_eq!(lite["output_lines"], 2);
    assert_eq!(lite["output_complete"], false);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(lite["output_base64"].as_str().expect("lite Base64"))
            .expect("Base64"),
        b"line1\nline2\x1b"
    );
    assert_eq!(decoded["items"][2]["output"], "line1\nline2");
    let semantic = &decoded["items"][3];
    assert!(semantic.get("message_id").is_none());
    assert!(semantic.get("text").is_none());
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(semantic["message_id_base64"].as_str().expect("message id"))
            .expect("Base64"),
        b"message\x1b"
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(semantic["text_base64"].as_str().expect("text"))
            .expect("Base64"),
        b"secret\x07"
    );
}

/// JSON and TOON preserve every relationship-family record without relying on
/// array position for journal ownership.
#[test]
fn toon_preserves_all_relationship_families() {
    let agent_id = AgentId::parse("agent-a").expect("agent");
    let header = Header {
        schema: SCHEMA,
        schema_version: 0,
        record_type: "header",
        root_agent_id: &agent_id,
        included_agent_ids: vec![&agent_id],
        content: "lite",
        time_unit: "microseconds",
        absolute_time: "unix_epoch_microseconds_at_journal_append_invocation",
        timing_basis: "producer_wall_clock_at_observation",
        causality: "explicit_observation_refs_only",
    };
    let records = vec![
        Record::Relationship(RelationshipRecord::WaitObservation(WaitObservationRecord {
            record_type: "relationship",
            relationship: "wait_observation",
            observation_id: id(1),
            wait_call: call(1),
            mode: tau_proto::ToolWaitMode::NextBackground,
        })),
        Record::Relationship(RelationshipRecord::WaitRegistration(
            WaitRegistrationRecord {
                record_type: "relationship",
                relationship: "wait_registration",
                observation_id: id(2),
                wait_observation: id(1),
                wait_call: call(1),
                mode: tau_proto::ToolWaitMode::NextBackground,
                registration: RegistrationState::Active,
                outcome: RegistrationOutcome::Settled,
            },
        )),
        Record::Relationship(RelationshipRecord::WaitSettlement(WaitSettlementRecord {
            record_type: "relationship",
            relationship: "wait_settlement",
            observation_id: id(3),
            wait_observation: id(1),
            wait_call: call(1),
            registration: RegistrationState::Active,
            registration_ref: Some(id(2)),
            wait_terminal: id(4),
            wait_terminal_resolution: Resolution::Resolved,
            outcome: WaitOutcomeRecord::Cancelled,
            active_wait_us: Some(1),
        })),
        Record::Relationship(RelationshipRecord::CancellationRequested(
            CancellationRequestedRecord {
                record_type: "relationship",
                relationship: "cancellation_requested",
                observation_id: id(5),
                cancel_call: call(5),
                target_call: call(1),
            },
        )),
    ];
    let records = timed_records(records);
    let mut expected = serde_json::to_value(&header).expect("header JSON");
    expected["items"] = serde_json::to_value(&records).expect("record JSON");
    let mut encoded = Vec::new();
    toon::write(&header, records, &mut encoded).expect("TOON");
    let actual: serde_json::Value =
        serde_toon::from_str(std::str::from_utf8(&encoded).expect("UTF-8")).expect("strict TOON");
    assert_eq!(actual, expected);
}

/// Placeholder terminals cannot masquerade as final output, and cancellation
/// classifications cannot cite a request that targets a different call.
#[test]
fn integrity_rejects_placeholder_terminals_and_wrong_cancellation_requests() {
    let placeholder = vec![
        declaration("agent-a", 1, 0, "call"),
        fact(
            "agent-a",
            2,
            1,
            2,
            Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                call: call(1),
                terminal: id(3),
                cause: tau_proto::ToolTerminalCause::Completed,
            }),
        ),
        fact(
            "agent-a",
            3,
            2,
            3,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call".into(),
                tool_name: tau_proto::ToolName::new("test"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Null,
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        ),
    ];
    assert_projection_error(placeholder, "does not own call");

    let wrong_request = vec![
        declaration("agent-a", 1, 0, "call"),
        fact(
            "agent-a",
            2,
            1,
            2,
            Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
                kind: tau_proto::ActivationKind::Other,
                source_observation: None,
                source_call: None,
            }),
        ),
        fact(
            "agent-a",
            3,
            2,
            3,
            Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
                call: call(1),
                terminal: id(4),
                cause: tau_proto::ToolTerminalCause::Cancellation { request: id(2) },
            }),
        ),
        fact(
            "agent-a",
            4,
            3,
            4,
            Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: "call".into(),
                tool_name: tau_proto::ToolName::new("test"),
                tool_type: tau_proto::ToolType::Function,
            }),
        ),
    ];
    assert_projection_error(wrong_request, "does not target");
}
