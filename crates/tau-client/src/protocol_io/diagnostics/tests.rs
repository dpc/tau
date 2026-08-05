use std::sync::Arc;

use tau_proto::{
    CborValue, Event, HarnessOutputMessage, PromptOriginator, ToolName, ToolType, UnixMicros,
};

use super::*;
use crate::protocol_io::{ProtocolIoFrameStats, message_len};

type ProtocolIoAttachPhase = AttachPhase;
type ProtocolIoDeliveryKind = DeliveryKind;
type ProtocolIoSizeDistribution = SizeDistribution;

/// Minimal adapter for exercising the private diagnostics owner directly.
#[derive(Default)]
struct TestMeter {
    /// Mutable diagnostic state under test.
    state: State,
}

impl TestMeter {
    fn record_downlink_frame(&mut self, message: &HarnessOutputMessage) {
        let bytes = message_len(message).expect("encode diagnostic fixture");
        let HarnessOutputMessage::Deliver(delivery) = message else {
            return;
        };
        let kind = if delivery.is_replay() {
            DeliveryKind::Replay
        } else {
            DeliveryKind::NonReplay
        };
        let event_key = delivery.event().name().to_string();
        let measurements = collect_measurements(message, bytes);
        self.state
            .record_delivery(kind, delivery.event(), &event_key, bytes, measurements);
    }
    fn diagnostic_stats(&self) -> Stats {
        self.state.stats.clone()
    }
    fn format_diagnostics(&self) -> String {
        self.state.format()
    }
}

impl std::ops::Index<&str> for Measurements {
    type Output = SizeDistribution;
    fn index(&self, label: &str) -> &Self::Output {
        self.0
            .iter()
            .find_map(|(key, distribution)| (key.label() == label).then_some(distribution))
            .expect("diagnostic measurement label")
    }
}
impl std::ops::Index<&String> for Measurements {
    type Output = SizeDistribution;
    fn index(&self, label: &String) -> &Self::Output {
        &self[label.as_str()]
    }
}
fn agent_id() -> tau_proto::AgentId {
    tau_proto::AgentId::parse("agent-1").expect("agent id")
}

fn finish_protocol_io_cold_attach(meter: &mut TestMeter) {
    meter.record_downlink_frame(&HarnessOutputMessage::deliver(
        Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            error: None,
        }),
    ));
}

fn protocol_test_encoded_len(value: &impl serde::Serialize) -> u64 {
    tau_proto::encode_message_to_vec(value)
        .expect("encode diagnostic fixture")
        .len() as u64
}

/// Downlink diagnostics split initial attach from steady traffic independently
/// of each delivery's replay marker.
#[test]
fn protocol_io_meter_splits_catch_up_from_steady_live() {
    let mut meter = TestMeter::default();
    let event = Event::TermBell(tau_proto::TermBell {});
    let replay = HarnessOutputMessage::deliver_replay(UnixMicros::new(1), event.clone());
    let live = HarnessOutputMessage::deliver_live(UnixMicros::new(2), event);
    let boundary = HarnessOutputMessage::deliver(Event::SessionReplayComplete(
        tau_proto::SessionReplayComplete {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            error: None,
        },
    ));

    meter.record_downlink_frame(&replay);
    meter.record_downlink_frame(&boundary);
    meter.record_downlink_frame(&live);
    meter.record_downlink_frame(&replay);

    let diagnostics = meter.diagnostic_stats();
    assert_eq!(
        diagnostics.downlink[&ProtocolIoAttachPhase::ColdAttach][&ProtocolIoDeliveryKind::Replay]
            ["term.bell"]
            .count,
        1
    );
    assert_eq!(
        diagnostics.downlink[&ProtocolIoAttachPhase::ColdAttach]
            [&ProtocolIoDeliveryKind::NonReplay]["session.replay_complete"],
        ProtocolIoFrameStats {
            count: 1,
            bytes: protocol_test_encoded_len(&boundary),
        }
    );
    assert!(
        diagnostics.downlink[&ProtocolIoAttachPhase::Steady]
            .values()
            .all(|events| !events.contains_key("session.replay_complete"))
    );
    assert_eq!(
        diagnostics.downlink[&ProtocolIoAttachPhase::Steady][&ProtocolIoDeliveryKind::NonReplay]
            ["term.bell"]
            .count,
        1
    );
    assert_eq!(
        diagnostics.downlink[&ProtocolIoAttachPhase::Steady][&ProtocolIoDeliveryKind::Replay]
            ["term.bell"]
            .count,
        1
    );
}

/// Size histograms retain exact totals/maxima and return documented inclusive
/// logarithmic percentile bounds at bucket edges.
#[test]
fn protocol_io_size_distribution_reports_bounded_percentiles() {
    let mut distribution = ProtocolIoSizeDistribution::default();
    for bytes in [0, 1, 2, 3, 4, 8, 9, 16, 17, 32] {
        distribution.record_bytes(bytes);
    }

    assert_eq!(distribution.count, 10);
    assert_eq!(distribution.bytes, 92);
    assert_eq!(distribution.max_bytes, 32);
    assert_eq!(distribution.percentile_upper_bound(50), 7);
    assert_eq!(distribution.percentile_upper_bound(95), 63);
    assert_eq!(distribution.percentile_upper_bound(99), 63);
}

/// Tool-result diagnostics retain only sizes and occurrence counts while
/// separating full event, raw result, display, and provider content payloads.
#[test]
fn protocol_io_meter_attributes_tool_result_fields_without_contents() {
    let mut meter = TestMeter::default();
    finish_protocol_io_cold_attach(&mut meter);
    let result = tau_proto::ToolResult {
        call_id: "call-1".into(),
        tool_name: ToolName::new("read"),
        tool_type: ToolType::Function,
        result: CborValue::Text("raw result contents".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: Arc::from(vec![1_u8, 2, 3]),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "README.md".to_owned(),
            ..Default::default()
        }),
        originator: PromptOriginator::User,
    };
    let first_event = Event::ToolResult(result.clone());
    let first_message = HarnessOutputMessage::deliver_live(UnixMicros::new(1), first_event.clone());
    let mut missing_display = result.clone();
    missing_display.display = None;
    missing_display.provider_content.clear();
    let second_event = Event::ToolResult(missing_display);
    let second_message =
        HarnessOutputMessage::deliver_live(UnixMicros::new(2), second_event.clone());
    meter.record_downlink_frame(&first_message);
    meter.record_downlink_frame(&second_message);

    let diagnostics = meter.diagnostic_stats();
    let key = "steady.non-replay.tool.result.encoded-frame";
    assert_eq!(diagnostics.measurements[key].count, 2, "{key}");
    assert_eq!(
        diagnostics.measurements["steady.non-replay.tool.result.encoded-frame"].bytes,
        protocol_test_encoded_len(&first_message) + protocol_test_encoded_len(&second_message)
    );
    assert_eq!(
        diagnostics.measurements["steady.non-replay.tool.result.display-present-frame"].count,
        1
    );
    assert_eq!(
        diagnostics.measurements["steady.non-replay.tool.result.display-missing-frame"].count,
        1
    );
    assert_eq!(
        diagnostics.measurements["steady.non-replay.tool.result.provider-content-missing-frame"]
            .count,
        1
    );
    assert_eq!(
        diagnostics.measurements["steady.non-replay.tool.result.provider-content-present-frame"]
            .count,
        1
    );
    assert!(!meter.format_diagnostics().contains("raw result contents"));
}

/// Background tool completions retain content-free frame/display attribution
/// without re-encoding their raw output.
#[test]
fn protocol_io_meter_attributes_background_tool_results() {
    let mut meter = TestMeter::default();
    let result = tau_proto::ToolBackgroundResult {
        call_id: "call-background".into(),
        tool_name: ToolName::new("shell"),
        tool_type: ToolType::Function,
        result: CborValue::Text("background output".to_owned()),
        display: Some(tau_proto::ToolUseState {
            args: "make check".to_owned(),
            ..Default::default()
        }),
        originator: PromptOriginator::User,
    };
    let event = Event::ToolBackgroundResult(result.clone());
    let message = HarnessOutputMessage::deliver_replay(UnixMicros::new(1), event.clone());
    meter.record_downlink_frame(&message);

    let measurements = meter.diagnostic_stats().measurements;
    let prefix = "cold-attach.replay.tool.background_result";
    assert_eq!(
        measurements[&format!("{prefix}.encoded-frame")].bytes,
        protocol_test_encoded_len(&message)
    );
    assert_eq!(
        measurements[&format!("{prefix}.display-present-frame")].count,
        1
    );
}

/// Provider-directed tool results retain their own event-name accounting when
/// an opt-in diagnostic meter observes that route.
#[test]
fn protocol_io_meter_attributes_provider_tool_results() {
    let mut meter = TestMeter::default();
    finish_protocol_io_cold_attach(&mut meter);
    let result = tau_proto::ToolResult {
        call_id: "call-provider".into(),
        tool_name: ToolName::new("read_image"),
        tool_type: ToolType::Function,
        result: CborValue::Text("provider result".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    };
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::ProviderToolResult(result.clone()),
    );
    meter.record_downlink_frame(&message);

    let measurements = meter.diagnostic_stats().measurements;
    let prefix = "steady.non-replay.provider.tool_result";
    assert_eq!(
        measurements[&format!("{prefix}.encoded-frame")].bytes,
        protocol_test_encoded_len(&message)
    );
    assert_eq!(
        measurements[&format!("{prefix}.display-missing-frame")].count,
        1
    );
    assert_eq!(
        measurements[&format!("{prefix}.provider-content-missing-frame")].count,
        1
    );
}

/// Final-response diagnostics retain frame totals without re-encoding semantic
/// output, provider sidecars, or metadata-only projections.
#[test]
fn protocol_io_meter_attributes_final_response_semantics_and_metadata() {
    let mut meter = TestMeter::default();
    let message_raw = r#"{"type":"message","provider_only":"sidecar"}"#.to_owned();
    let raw_arguments = r#"{"path":"README.md"}"#.to_owned();
    let envelope = tau_proto::ResponsesToolCallEnvelope {
        item_id: Some("item-1".to_owned()),
        status: Some("completed".to_owned()),
        extra_fields: Some(CborValue::Map(vec![(
            CborValue::Text("provider".to_owned()),
            CborValue::Text("extra".to_owned()),
        )])),
    };
    let reasoning = tau_proto::OpaqueProviderItem::with_raw_json(
        CborValue::Text("reasoning-value".to_owned()),
        r#"{"type":"reasoning"}"#,
    );
    let compaction = tau_proto::OpaqueProviderItem::new(CborValue::Text("compact".to_owned()));
    let unknown = tau_proto::OpaqueProviderItem::new(CborValue::Text("unknown".to_owned()));
    let message = tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: "semantic response".to_owned(),
        }],
        phase: None,
        responses_raw_json: Some(message_raw.clone()),
    };
    let tool_call = tau_proto::ToolCallItem {
        call_id: "call-1".into(),
        name: ToolName::new("read"),
        tool_type: ToolType::Function,
        arguments: CborValue::Map(vec![]),
        raw_arguments_json: Some(raw_arguments.clone()),
        responses_envelope: Some(envelope.clone()),
    };
    let tool_result = tau_proto::ToolResultItem {
        call_id: "call-1".into(),
        tool_type: ToolType::Function,
        status: tau_proto::ToolResultStatus::Success,
        output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("result".to_owned())),
        provider_content: Vec::new(),
    };
    let reasoning_text = tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Summary,
        text: "summary".to_owned(),
    };
    let output_items = vec![
        tau_proto::ContextItem::Message(message.clone()),
        tau_proto::ContextItem::ToolCall(tool_call.clone()),
        tau_proto::ContextItem::ToolResult(tool_result.clone()),
        tau_proto::ContextItem::ReasoningText(reasoning_text.clone()),
        tau_proto::ContextItem::Reasoning(reasoning.clone()),
        tau_proto::ContextItem::CompactionTrigger,
        tau_proto::ContextItem::Compaction(compaction.clone()),
        tau_proto::ContextItem::UnknownProviderItem(unknown.clone()),
    ];
    let response = tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: "prompt-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: agent_id(),
        output_items,
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: Some("display metadata".to_owned()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: Some(100),
        compaction_compacted_input_tokens: Some(25),
        backend: None,
        provider_response_id: Some("response-1".to_owned()),
        ws_pool_delta: None,
    };
    let event = Event::ProviderResponseFinished(response.clone());
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1),
        event.clone(),
    ));

    let measurements = meter.diagnostic_stats().measurements;
    let prefix = "cold-attach.replay.provider.response_finished";
    assert!(
        measurements
            .iter()
            .all(|(key, _)| !key.label().starts_with(prefix)),
        "provider response components must not be re-encoded"
    );
}

/// Exact stats duplicate classification resets at a loaded-agent boundary so a
/// reloaded agent's first snapshot is never charged to its previous epoch.
#[test]
fn protocol_io_meter_counts_exact_stats_duplicates_per_loaded_epoch() {
    let mut meter = TestMeter::default();
    finish_protocol_io_cold_attach(&mut meter);
    let stats = tau_proto::AgentStatsUpdated {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id: agent_id(),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    let stats_message = || {
        HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            Event::AgentStatsUpdated(stats.clone()),
        )
    };
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id(),
        }),
    ));
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
        UnixMicros::new(3),
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id(),
            ephemeral: false,
        }),
    ));
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&stats_message());
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
        UnixMicros::new(4),
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "session-2"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            reason: tau_proto::SessionStartReason::Initial,
        }),
    ));
    meter.record_downlink_frame(&stats_message());

    let diagnostics = meter.diagnostic_stats();
    assert_eq!(
        diagnostics.measurements
            ["steady.non-replay.agent.stats_updated.exact-duplicate-within-loaded-epoch-frame"]
            .count,
        3
    );
    assert_eq!(
        diagnostics.measurements["steady.non-replay.agent.stats_updated.initial-loaded-epoch-frame"]
            .count,
        4
    );
}

/// Replay snapshots must not become duplicate predecessors for the first live
/// snapshot after cold attach.
#[test]
fn protocol_io_meter_separates_equality_caches_by_attach_and_delivery_kind() {
    let mut meter = TestMeter::default();
    let stats = tau_proto::AgentStatsUpdated {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id: agent_id(),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1),
        Event::AgentStatsUpdated(stats.clone()),
    ));
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
        UnixMicros::new(2),
        Event::AgentStatsUpdated(stats.clone()),
    ));
    finish_protocol_io_cold_attach(&mut meter);
    for recorded_at in [3, 4] {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
            UnixMicros::new(recorded_at),
            Event::AgentStatsUpdated(stats.clone()),
        ));
    }
    for recorded_at in [5, 6] {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(recorded_at),
            Event::AgentStatsUpdated(stats.clone()),
        ));
    }

    let measurements = meter.diagnostic_stats().measurements;
    assert_eq!(
        measurements["cold-attach.replay.agent.stats_updated.initial-loaded-epoch-frame"].count,
        1
    );
    assert_eq!(
        measurements
            ["cold-attach.replay.agent.stats_updated.exact-duplicate-within-loaded-epoch-frame"]
            .count,
        1
    );
    assert_eq!(
        measurements["steady.replay.agent.stats_updated.initial-loaded-epoch-frame"].count,
        1
    );
    assert_eq!(
        measurements["steady.replay.agent.stats_updated.exact-duplicate-within-loaded-epoch-frame"]
            .count,
        1
    );
    assert_eq!(
        measurements["steady.non-replay.agent.stats_updated.initial-loaded-epoch-frame"].count,
        1
    );
    assert_eq!(
        measurements
            ["steady.non-replay.agent.stats_updated.exact-duplicate-within-loaded-epoch-frame"]
            .count,
        1
    );
}

/// Quota equality uses the same independent attach and replay dimensions as
/// agent stats.
#[test]
fn protocol_io_meter_separates_quota_equality_caches_by_both_axes() {
    let mut meter = TestMeter::default();
    let quota = tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("provider"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: 1,
        windows: Vec::new(),
        route_bindings: Vec::new(),
    };
    for recorded_at in [1, 2] {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
            UnixMicros::new(recorded_at),
            Event::HarnessProviderQuotaChanged(quota.clone()),
        ));
    }
    finish_protocol_io_cold_attach(&mut meter);
    for recorded_at in [3, 4] {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_replay(
            UnixMicros::new(recorded_at),
            Event::HarnessProviderQuotaChanged(quota.clone()),
        ));
    }
    for recorded_at in [5, 6] {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(recorded_at),
            Event::HarnessProviderQuotaChanged(quota.clone()),
        ));
    }

    let measurements = meter.diagnostic_stats().measurements;
    for prefix in ["cold-attach.replay", "steady.replay", "steady.non-replay"] {
        assert_eq!(
            measurements
                [&format!("{prefix}.harness.provider_quota_changed.initial-snapshot-frame")]
                .count,
            1,
            "{prefix}"
        );
        assert_eq!(
            measurements[&format!("{prefix}.harness.provider_quota_changed.exact-duplicate-frame")]
                .count,
            1,
            "{prefix}"
        );
    }
}

/// Quota diagnostics distinguish exact equality, sequence-only movement, and
/// substantive current-state changes without suppressing any snapshot.
#[test]
fn protocol_io_meter_classifies_quota_snapshot_changes() {
    let mut meter = TestMeter::default();
    finish_protocol_io_cold_attach(&mut meter);
    let mut quota = tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("provider"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: 1,
        windows: Vec::new(),
        route_bindings: Vec::new(),
    };
    let record = |meter: &mut TestMeter, quota: &tau_proto::HarnessProviderQuotaChanged| {
        meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            Event::HarnessProviderQuotaChanged(quota.clone()),
        ));
    };
    record(&mut meter, &quota);
    record(&mut meter, &quota);
    quota.sequence = 2;
    record(&mut meter, &quota);
    quota.profile_epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-2").expect("epoch");
    record(&mut meter, &quota);
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "session-2"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            reason: tau_proto::SessionStartReason::Initial,
        }),
    ));
    record(&mut meter, &quota);

    let measurements = meter.diagnostic_stats().measurements;
    for classification in [
        "initial-snapshot-frame",
        "exact-duplicate-frame",
        "sequence-only-change-frame",
        "substantive-change-frame",
    ] {
        let expected = u64::from(classification == "initial-snapshot-frame") + 1;
        assert_eq!(
            measurements
                [&format!("steady.non-replay.harness.provider_quota_changed.{classification}")]
                .count,
            expected,
            "{classification}"
        );
    }
}
