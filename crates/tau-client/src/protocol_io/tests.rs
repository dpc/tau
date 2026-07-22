use tau_proto::{
    CborValue, Event, HarnessInputMessage, HarnessOutputMessage, PromptOriginator, ToolName,
    ToolType, UnixMicros,
};

use super::*;

/// Protocol I/O keys should identify delivered events by their inner event name
/// so generic stats point at the event family that generated traffic, not the
/// transport envelope.
#[test]
fn protocol_io_output_message_key_uses_delivered_event_name() {
    let message = HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(output_message_key(&message), "term.bell");
}

/// Protocol I/O keys should identify emitted events by their inner event name
/// so peer-originated event traffic is grouped consistently with delivered
/// traffic.
#[test]
fn protocol_io_input_message_key_uses_emitted_event_name() {
    let message = HarnessInputMessage::emit(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(input_message_key(&message), "term.bell");
}

/// Dedicated extension notice requests use their flat message name for
/// metering.
#[test]
fn protocol_io_input_message_key_uses_extension_notice_request_name() {
    let message = HarnessInputMessage::ExtensionNoticeRequest(tau_proto::ExtensionNoticeRequest {
        message: "reconnecting".to_owned(),
        level: tau_proto::NoticeLevel::Warning,
    });

    assert_eq!(
        input_message_key(&message),
        "message.extension_notice_request"
    );
}

/// The separate harness-authored output remains metered as its event name.
#[test]
fn protocol_io_output_message_key_uses_harness_notice_name() {
    let message =
        HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice::new(
            tau_proto::notice_kind::EXTENSION_NOTICE,
            "reconnecting",
            tau_proto::NoticeLevel::Warning,
        )));

    assert_eq!(output_message_key(&message), "harness.notice");
}

/// Dedicated UI debug requests use a flat message key rather than the removed
/// dotted event name.
#[test]
fn protocol_io_input_message_key_uses_ui_debug_request_message_name() {
    let message =
        HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
            extension_name: "std-shell".into(),
        });

    assert_eq!(
        input_message_key(&message),
        "message.ui_debug_event_stats_request"
    );
}

/// Dedicated UI detach requests use their flat message name for metering.
#[test]
fn protocol_io_input_message_key_uses_ui_detach_request_message_name() {
    let message = HarnessInputMessage::UiDetachRequest(tau_proto::UiDetachRequest::default());

    assert_eq!(input_message_key(&message), "message.ui_detach_request");
}

/// Dedicated UI tree requests use their flat message name for metering.
#[test]
fn protocol_io_input_message_key_uses_ui_tree_request_message_name() {
    let message = HarnessInputMessage::UiTreeRequest(tau_proto::UiTreeRequest {
        session_id: "s1".into(),
        target_agent_id: None,
    });

    assert_eq!(input_message_key(&message), "message.ui_tree_request");
}

/// Cumulative protocol I/O counters must survive sample draining because debug
/// dumps are lifetime counters while rolling samples drive transient status.
#[test]
fn protocol_io_meter_keeps_cumulative_stats_after_sampling() {
    let meter = ProtocolIoMeter::default();
    meter.record_bytes(
        ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(10),
    );
    meter.record_bytes(
        ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(15),
    );

    let sample = meter.take_sample();

    assert_eq!(sample.downlink_bytes, 25);
    assert_eq!(
        meter
            .cumulative_stats()
            .downlink
            .get("small.event")
            .copied(),
        Some(ProtocolIoFrameStats {
            count: 2,
            bytes: 25
        })
    );
}

/// Detailed payload encoding is explicit opt-in so extension meters preserve
/// their cumulative-only hot path.
#[test]
fn protocol_io_default_meter_skips_detailed_diagnostics() {
    let meter = ProtocolIoMeter::default();
    meter.record_downlink_frame(&HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    ));

    assert!(meter.format_diagnostics().is_empty());
    assert_eq!(meter.cumulative_stats().downlink["term.bell"].count, 1);
}

/// The opt-in meter wires exact frame sizes, attach/replay axes, selected
/// payload attribution, and stable cumulative totals into one public report.
#[test]
fn protocol_io_detailed_meter_integrates_delivery_and_cumulative_accounting() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let replay = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let boundary = HarnessOutputMessage::deliver(Event::SessionReplayComplete(
        tau_proto::SessionReplayComplete {
            session_id: "session-1".into(),
            error: None,
        },
    ));
    let live = HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let steady_replay = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(3),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let tool = HarnessOutputMessage::deliver_live(
        UnixMicros::new(4),
        Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("read"),
            tool_type: ToolType::Function,
            result: CborValue::Text("raw".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
    );
    for message in [&replay, &boundary, &live, &steady_replay, &tool] {
        meter.record_downlink_frame(message);
    }

    let encoded_len = |message: &HarnessOutputMessage| {
        tau_proto::encode_message_to_vec(message)
            .expect("encode integration fixture")
            .len() as u64
    };
    assert_eq!(meter.cumulative_stats().downlink["term.bell"].count, 3);
    assert_eq!(meter.cumulative_stats().downlink["tool.result"].count, 1);
    let formatted = meter.format_diagnostics();
    assert!(formatted.contains(&format!(
        "cold-attach.replay: bytes={} count=1",
        encoded_len(&replay)
    )));
    assert!(formatted.contains(&format!(
        "cold-attach.non-replay: bytes={} count=1",
        encoded_len(&boundary)
    )));
    assert!(formatted.contains(&format!(
        "steady.replay: bytes={} count=1",
        encoded_len(&steady_replay)
    )));
    assert!(formatted.contains(&format!(
        "steady.non-replay: bytes={} count=2",
        encoded_len(&live) + encoded_len(&tool)
    )));
    assert!(formatted.contains(&format!(
        "steady.non-replay.tool.result.encoded-frame: bytes={} count=1",
        encoded_len(&tool)
    )));
}

/// Protocol I/O meters must bound distinct keys so a peer cannot grow harness
/// memory forever by emitting unique custom event names.
#[test]
fn protocol_io_meter_buckets_overflow_after_key_cap() {
    let meter = ProtocolIoMeter::default();
    for index in 0..(PROTOCOL_IO_MAX_KEYS_PER_DIRECTION + 4) {
        meter.record_bytes(
            ProtocolIoDirection::Uplink,
            format!("custom.event_{index}"),
            Some(1),
        );
    }

    let sample = meter.take_sample();
    let cumulative = meter.cumulative_stats();

    assert_eq!(
        sample.uplink_breakdown.len(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION
    );
    assert_eq!(cumulative.uplink.len(), PROTOCOL_IO_MAX_KEYS_PER_DIRECTION);
    assert_eq!(
        sample.uplink_breakdown.get(PROTOCOL_IO_OVERFLOW_KEY),
        Some(&5)
    );
    assert_eq!(
        cumulative.uplink.get(PROTOCOL_IO_OVERFLOW_KEY).copied(),
        Some(ProtocolIoFrameStats { count: 5, bytes: 5 })
    );
}

/// Human-readable protocol I/O stats should use stable labels supplied by the
/// caller so UI and extension debug dumps can share accounting without sharing
/// perspective-specific wording.
#[test]
fn protocol_io_cumulative_stats_format_uses_labels_and_sorting() {
    let mut stats = ProtocolIoCumulativeStats::default();
    stats.uplink.insert(
        "message.hello".to_owned(),
        ProtocolIoFrameStats {
            count: 1,
            bytes: 50,
        },
    );
    stats.downlink.insert(
        "small.event".to_owned(),
        ProtocolIoFrameStats {
            count: 3,
            bytes: 512,
        },
    );
    stats.downlink.insert(
        "large.event".to_owned(),
        ProtocolIoFrameStats {
            count: 2,
            bytes: 12 * 1024,
        },
    );

    let formatted = format_protocol_io_cumulative_stats(
        "Example stats",
        "peer -> harness",
        "harness -> peer",
        "empty",
        &stats,
    );

    assert!(formatted.contains("peer -> harness: 50B in 1 frame(s)"));
    assert!(formatted.contains("  message.hello: 50B count=1"));
    assert!(formatted.contains("harness -> peer: 12K in 5 frame(s)"));
    assert!(!formatted.contains("bytes="));
    assert!(
        formatted.find("large.event").expect("large event line")
            < formatted.find("small.event").expect("small event line")
    );
}
