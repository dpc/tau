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

/// Flat input variants must retain their protocol names so each non-`Emit`
/// message-key match arm stays distinguishable without duplicating fixtures.
#[test]
fn protocol_io_input_message_keys_use_flat_variant_names() {
    let cases = [
        (
            HarnessInputMessage::ExtensionNoticeRequest(tau_proto::ExtensionNoticeRequest {
                message: "reconnecting".to_owned(),
                level: tau_proto::NoticeLevel::Warning,
            }),
            "message.extension_notice_request",
        ),
        (
            HarnessInputMessage::UiDebugEventStatsRequest(tau_proto::UiDebugEventStatsRequest {
                extension_name: test_extension_name("std-shell"),
            }),
            "message.ui_debug_event_stats_request",
        ),
        (
            HarnessInputMessage::UiDetachRequest(tau_proto::UiDetachRequest::default()),
            "message.ui_detach_request",
        ),
        (
            HarnessInputMessage::UiTreeRequest(tau_proto::UiTreeRequest {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                target_agent_id: None,
            }),
            "message.ui_tree_request",
        ),
    ];

    for (message, expected_key) in cases {
        assert_eq!(
            input_message_key(&message),
            expected_key,
            "message key must remain {expected_key}"
        );
    }
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

/// Detailed content-free classification is explicit opt-in so extension meters
/// preserve their cumulative-only hot path.
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

/// Ensures decoded-frame accounting trusts the transport size supplied by the
/// codec instead of re-encoding the message to derive another value.
#[test]
fn protocol_io_meter_attributes_supplied_decoded_frame_size() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );

    meter.record_downlink_frame_bytes(
        &message,
        tau_proto::ProtocolMessageBytes::new(12_345).expect("nonzero fixture size"),
    );

    assert_eq!(
        meter.cumulative_stats().downlink["term.bell"],
        ProtocolIoFrameStats {
            count: 1,
            bytes: 12_345,
        }
    );
    assert!(meter.format_diagnostics().contains("bytes=12345 count=1"));
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
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
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
            presentation: Default::default(),
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

/// Protocol I/O meters must cap each direction independently so configured
/// extensions' custom names cannot grow debug-accounting state without bound.
#[test]
fn protocol_io_meter_buckets_overflow_after_key_cap() {
    let meter = ProtocolIoMeter::default();
    for direction in [ProtocolIoDirection::Uplink, ProtocolIoDirection::Downlink] {
        for index in 0..(PROTOCOL_IO_MAX_KEYS_PER_DIRECTION + 4) {
            meter.record_bytes(direction, format!("custom.event_{index}"), Some(1));
        }
    }

    let sample = meter.take_sample();
    let cumulative = meter.cumulative_stats();

    for (direction, rolling, total) in [
        ("uplink", &sample.uplink_breakdown, &cumulative.uplink),
        ("downlink", &sample.downlink_breakdown, &cumulative.downlink),
    ] {
        assert_eq!(
            rolling.len(),
            PROTOCOL_IO_MAX_KEYS_PER_DIRECTION,
            "{direction} rolling keys must remain capped"
        );
        assert_eq!(
            total.len(),
            PROTOCOL_IO_MAX_KEYS_PER_DIRECTION,
            "{direction} cumulative keys must remain capped"
        );
        assert_eq!(
            rolling.get(PROTOCOL_IO_OVERFLOW_KEY),
            Some(&5),
            "{direction} rolling overflow must retain all excess frames"
        );
        assert_eq!(
            total.get(PROTOCOL_IO_OVERFLOW_KEY).copied(),
            Some(ProtocolIoFrameStats { count: 5, bytes: 5 }),
            "{direction} cumulative overflow must retain all excess frames"
        );
    }
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

    assert_eq!(
        formatted,
        "\
Example stats
peer -> harness: 50B in 1 frame(s)
  message.hello: 50B count=1
harness -> peer: 12K in 5 frame(s)
  large.event: 12K count=2
  small.event: 512B count=3"
    );
}

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}
