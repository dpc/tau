use std::collections::BTreeMap;
use std::time::Instant;

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
            HarnessInputMessage::UiShutdownRequest(tau_proto::UiShutdownRequest::default()),
            "message.ui_shutdown_request",
        ),
        (
            HarnessInputMessage::UiQuitRequest(tau_proto::UiQuitRequest::default()),
            "message.ui_quit_request",
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

/// The first detailed delivery must allocate only the retained event key and
/// the three bounded accounting entries that own it.
#[test]
fn protocol_io_cold_detailed_delivery_allocates_retained_keys() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );

    assert_eq!(take_protocol_io_key_allocations(), 0);
    meter.record_downlink_frame_bytes(
        &message,
        tau_proto::ProtocolMessageBytes::new(17).expect("nonzero fixture size"),
    );

    assert_eq!(take_protocol_io_key_allocations(), 4);
    assert_eq!(
        meter.cumulative_stats().downlink["term.bell"],
        ProtocolIoFrameStats {
            count: 1,
            bytes: 17,
        }
    );
    assert_eq!(
        meter.format_diagnostics(),
        concat!(
            "Downlink attach phase x delivery kind (exact encoded frame bytes)\n",
            "cold-attach.replay: bytes=0 count=0\n",
            "cold-attach.non-replay: bytes=17 count=1\n",
            "  term.bell: bytes=17 count=1\n",
            "steady.replay: bytes=0 count=0\n",
            "steady.non-replay: bytes=0 count=0"
        )
    );
}

/// Warm cumulative accounting must update both retained maps without creating
/// another event-name string.
#[test]
fn protocol_io_warm_cumulative_delivery_borrows_retained_event_key() {
    let meter = ProtocolIoMeter::default();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let bytes = tau_proto::ProtocolMessageBytes::new(17).expect("nonzero fixture size");
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 3);
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 0);
    assert_eq!(
        meter.cumulative_stats().downlink["term.bell"],
        ProtocolIoFrameStats {
            count: 2,
            bytes: 34,
        }
    );
}

/// Warm detailed accounting must borrow the shared event key for cumulative,
/// rolling, and diagnostic maps rather than recreating four equal strings.
#[test]
fn protocol_io_warm_detailed_delivery_borrows_all_event_keys() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let bytes = tau_proto::ProtocolMessageBytes::new(17).expect("nonzero fixture size");
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 4);
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 0);
    assert_eq!(
        meter.cumulative_stats().downlink["term.bell"],
        ProtocolIoFrameStats {
            count: 2,
            bytes: 34,
        }
    );
    assert!(
        meter
            .format_diagnostics()
            .contains("  term.bell: bytes=34 count=2")
    );

    let sample = meter.take_sample();
    assert_eq!(sample.downlink_breakdown["term.bell"], 34);
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 1);
    assert_eq!(
        meter.take_sample().downlink_breakdown,
        BTreeMap::from([("term.bell".to_owned(), 17)])
    );
    assert_eq!(
        meter.cumulative_stats().downlink["term.bell"],
        ProtocolIoFrameStats {
            count: 3,
            bytes: 51,
        }
    );
    assert!(
        meter
            .format_diagnostics()
            .contains("  term.bell: bytes=51 count=3")
    );
}

/// Warm extension-owned deliveries must borrow their stored `EventName` rather
/// than cloning its dynamically owned segments before the cache lookup.
#[test]
fn protocol_io_warm_detailed_custom_delivery_borrows_event_name() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(
                "custom.bell"
                    .parse()
                    .expect("custom event name must be valid"),
                None,
                CborValue::Null,
            )
            .expect("custom event name must use an extension-owned category"),
        ),
    );
    let bytes = tau_proto::ProtocolMessageBytes::new(17).expect("nonzero fixture size");
    assert_eq!(take_protocol_io_owned_event_names(), 0);
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 4);
    assert_eq!(take_protocol_io_owned_event_names(), 0);
    meter.record_downlink_frame_bytes(&message, bytes);

    assert_eq!(take_protocol_io_key_allocations(), 0);
    assert_eq!(take_protocol_io_owned_event_names(), 0);
    assert_eq!(
        meter.cumulative_stats().downlink["custom.bell"],
        ProtocolIoFrameStats {
            count: 2,
            bytes: 34,
        }
    );
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

/// A full direction must select its existing `other` bucket while preserving
/// the cap, exact cumulative counters, and stable formatted labels.
#[test]
fn protocol_io_meter_overflow_selection_allocates_only_new_retained_keys() {
    let meter = ProtocolIoMeter::default();
    for index in 0..PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1) {
        meter.record_bytes(
            ProtocolIoDirection::Downlink,
            format!("custom.event_{index}"),
            Some(1),
        );
    }
    let overflow = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );

    assert_eq!(
        take_protocol_io_key_allocations(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1) * 2
    );
    meter.record_downlink_frame_bytes(
        &overflow,
        tau_proto::ProtocolMessageBytes::new(7).expect("nonzero fixture size"),
    );

    assert_eq!(take_protocol_io_key_allocations(), 3);
    let cumulative = meter.cumulative_stats();
    assert_eq!(
        cumulative.downlink.len(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION
    );
    assert_eq!(
        cumulative.downlink[PROTOCOL_IO_OVERFLOW_KEY],
        ProtocolIoFrameStats { count: 1, bytes: 7 }
    );
    assert!(
        format_protocol_io_cumulative_stats(
            "Protocol I/O",
            "uplink",
            "downlink",
            "none",
            &cumulative,
        )
        .contains("\n  other: 7B count=1")
    );
}

/// Delivered events that overflow cumulative storage must retain only accepted
/// cache keys while each drained rolling bucket admits its own bounded keys.
#[test]
fn protocol_io_delivered_overflow_keeps_cache_bounded_across_sample_drains() {
    let meter = ProtocolIoMeter::with_diagnostics();
    for index in 0..PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1) {
        meter.record_downlink_frame_bytes(
            &custom_delivery(index),
            tau_proto::ProtocolMessageBytes::new(1).expect("nonzero fixture size"),
        );
    }
    assert_eq!(
        meter.cached_event_key_count(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1)
    );
    assert_eq!(
        meter.take_sample().downlink_breakdown.len(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1)
    );
    take_protocol_io_key_allocations();

    meter.record_downlink_frame_bytes(
        &custom_delivery(PROTOCOL_IO_MAX_KEYS_PER_DIRECTION),
        tau_proto::ProtocolMessageBytes::new(7).expect("nonzero fixture size"),
    );

    assert_eq!(take_protocol_io_key_allocations(), 4);
    assert_eq!(
        meter.cached_event_key_count(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1)
    );
    assert_eq!(
        meter.take_sample().downlink_breakdown,
        BTreeMap::from([("custom.event_128".to_owned(), 7)])
    );
    assert_eq!(
        meter.cumulative_stats().downlink[PROTOCOL_IO_OVERFLOW_KEY],
        ProtocolIoFrameStats { count: 1, bytes: 7 }
    );
    assert!(
        meter
            .format_diagnostics()
            .contains("  other: bytes=7 count=1")
    );

    meter.record_downlink_frame_bytes(
        &custom_delivery(PROTOCOL_IO_MAX_KEYS_PER_DIRECTION + 1),
        tau_proto::ProtocolMessageBytes::new(7).expect("nonzero fixture size"),
    );

    assert_eq!(take_protocol_io_key_allocations(), 2);
    assert_eq!(
        meter.cached_event_key_count(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1)
    );
    assert_eq!(
        meter.cumulative_stats().downlink[PROTOCOL_IO_OVERFLOW_KEY],
        ProtocolIoFrameStats {
            count: 2,
            bytes: 14,
        }
    );
    assert!(
        meter
            .format_diagnostics()
            .contains("  other: bytes=14 count=2")
    );
}

/// This manual benchmark reports cold and warm event-key allocation counts and
/// elapsed warm-delivery time without making timing a correctness oracle.
#[test]
#[ignore = "manual protocol-I/O event-key allocation benchmark"]
fn benchmark_protocol_io_event_key_allocations() {
    let meter = ProtocolIoMeter::with_diagnostics();
    let message = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        Event::TermBell(tau_proto::TermBell {}),
    );
    let bytes = tau_proto::ProtocolMessageBytes::new(17).expect("nonzero fixture size");

    meter.record_downlink_frame_bytes(&message, bytes);
    let cold_allocations = take_protocol_io_key_allocations();
    let started = Instant::now();
    for _ in 0..10_000 {
        meter.record_downlink_frame_bytes(&message, bytes);
    }
    let elapsed = started.elapsed();
    let warm_allocations = take_protocol_io_key_allocations();
    eprintln!(
        "protocol I/O event-key benchmark: cold_allocations={cold_allocations} warm_allocations={warm_allocations} warm_deliveries=10000 elapsed={elapsed:?}"
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

/// Build one extension-owned delivery with a distinct custom event name.
fn custom_delivery(index: usize) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_live(
        UnixMicros::new(index as u64),
        Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(
                format!("custom.event_{index}")
                    .parse()
                    .expect("custom event name must be valid"),
                None,
                CborValue::Null,
            )
            .expect("custom event name must use an extension-owned category"),
        ),
    )
}
