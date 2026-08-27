use std::sync::Arc;

use tau_core::AgentEntry;
use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextLimitObservation, ContextRole, ImageContent,
    ImageDetail, ImageMediaType, MessageItem, OpaqueProviderItem, ToolCallItem,
    ToolResultContentPart, ToolResultItem, ToolResultStatus, ToolType,
};

use super::context_limit_telemetry::{
    context_limit_observation, projected_input_tokens, projected_transcript_entry_tokens,
    serialized_transcript_delta_bytes, serialized_transcript_entry_bytes, transcript_growth,
};

fn user_entry(text: &str) -> AgentEntry {
    AgentEntry::UserInput {
        items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        submission_source: None,
        inference_activation: true,
    }
}

fn tool_result_entry(raw_bytes: usize, provider_bytes: usize) -> AgentEntry {
    AgentEntry::ToolResults {
        items: vec![ToolResultItem {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse {
                raw: CborValue::Text("r".repeat(raw_bytes)),
                headers: Vec::new(),
                body: "p".repeat(provider_bytes),
            },
            provider_content: Vec::new(),
        }],
    }
}

fn assistant_entry(item: ContextItem) -> AgentEntry {
    AgentEntry::AssistantResponse {
        provider_response_id: None,
        backend: None,
        output_items: vec![item],
        usage: None,
    }
}

/// Below-limit agreement exposes hidden overhead or provider drift.
#[test]
fn rejection_below_advertised_limit_is_visible() {
    assert_eq!(
        context_limit_observation(Some(127_000), Some(126_000), Some(128_000)),
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    );
}

/// Provider usage remains sufficient evidence when no optional conservative
/// projection is available.
#[test]
fn provider_usage_without_projection_is_categorical() {
    assert_eq!(
        context_limit_observation(Some(127_000), None, Some(128_000)),
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    );
    assert_eq!(
        context_limit_observation(Some(128_000), None, Some(128_000)),
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    );
}

/// Missing, zero, or contradictory evidence must not manufacture capacity.
#[test]
fn invalid_or_contradictory_evidence_is_insufficient() {
    for observation in [
        context_limit_observation(Some(127_000), None, None),
        context_limit_observation(Some(0), Some(127_000), Some(128_000)),
        context_limit_observation(None, Some(127_000), Some(128_000)),
        context_limit_observation(None, Some(129_000), Some(128_000)),
        context_limit_observation(Some(129_000), Some(127_000), Some(128_000)),
    ] {
        assert_eq!(observation, ContextLimitObservation::InsufficientEvidence);
    }
}

/// Agreement at or above the advertised window is classified distinctly from
/// hidden-overhead drift.
#[test]
fn rejection_at_or_above_limit_is_visible() {
    assert_eq!(
        context_limit_observation(Some(130_000), Some(129_000), Some(128_000)),
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    );
}

/// The production transcript-growth derivation must count ordinary ASCII as
/// serialized bytes rather than applying an undocumented token conversion.
#[test]
fn transcript_delta_derivation_counts_ascii_json_bytes() {
    let one = serialized_transcript_entry_bytes(&user_entry("a")).expect("JSON-representable");
    let four = serialized_transcript_entry_bytes(&user_entry("abcd")).expect("JSON-representable");

    assert_eq!(four - one, 3);
}

/// The production transcript-growth derivation must retain UTF-8 byte
/// provenance: a two-byte scalar adds one byte relative to one ASCII scalar.
#[test]
fn transcript_delta_derivation_counts_multibyte_utf8() {
    let ascii = serialized_transcript_entry_bytes(&user_entry("a")).expect("JSON-representable");
    let utf8 = serialized_transcript_entry_bytes(&user_entry("é")).expect("JSON-representable");

    assert_eq!(utf8 - ascii, 1);
}

/// The exact serialized transcript-growth telemetry must include JSON escaping
/// so operators can reproduce its durable byte provenance.
#[test]
fn transcript_delta_derivation_counts_json_escaping() {
    let plain = serialized_transcript_entry_bytes(&user_entry("a")).expect("JSON-representable");
    let quote = serialized_transcript_entry_bytes(&user_entry("\"")).expect("JSON-representable");
    let newline = serialized_transcript_entry_bytes(&user_entry("\n")).expect("JSON-representable");

    assert_eq!(quote - plain, 1);
    assert_eq!(newline - plain, 1);
}

/// A supported raw-CBOR tool result with a non-string map key must retain exact
/// JSON-envelope provenance rather than emit a sentinel.
#[test]
fn transcript_delta_derivation_handles_raw_cbor_without_sentinel() {
    let raw = CborValue::Map(vec![(
        CborValue::Integer(1.into()),
        CborValue::Text("value".to_owned()),
    )]);
    let entry = AgentEntry::ToolResults {
        items: vec![ToolResultItem {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&raw),
            provider_content: Vec::new(),
        }],
    };

    let entry_bytes =
        serialized_transcript_entry_bytes(&entry).expect("CBOR value has a JSON envelope");
    let valid = user_entry("valid");
    assert_eq!(
        serialized_transcript_delta_bytes([&valid, &entry]),
        Some(serialized_transcript_entry_bytes(&valid).expect("valid entry") + entry_bytes)
    );
}

/// Typed image projection must count canonical bytes and 32-by-32 patches once,
/// rather than treating serde's expanded JSON integer array as provider tokens.
#[test]
fn typed_image_projection_uses_canonical_bytes_and_patches() {
    let image_bytes = vec![0x80; 116_573];
    let entry = AgentEntry::ToolResults {
        items: vec![ToolResultItem {
            presentation: Default::default(),
            call_id: "call-image".into(),
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                "bounded image".to_owned(),
            )),
            provider_content: vec![ToolResultContentPart::Image(ImageContent {
                media_type: ImageMediaType::Png,
                data: Arc::from(image_bytes.clone()),
                width: 1280,
                height: 900,
                detail: ImageDetail::High,
            })],
        }],
    };
    let mut metadata_only = entry.clone();
    let AgentEntry::ToolResults { items } = &mut metadata_only else {
        unreachable!("fixture is a tool result")
    };
    tau_proto::clear_tool_result_provider_image_bytes(&mut items[0]);
    items[0].output.body = items[0].output.render();
    items[0].output.headers.clear();
    items[0].output.raw = CborValue::Null;
    let metadata_tokens =
        serialized_transcript_entry_bytes(&metadata_only).expect("metadata serializes");
    let patch_tokens = 1280_u64.div_ceil(32) * 900_u64.div_ceil(32);
    let projected = projected_transcript_entry_tokens(&entry).expect("projection");

    assert_eq!(
        projected,
        metadata_tokens + image_bytes.len() as u64 + patch_tokens
    );
    assert!(
        projected < serialized_transcript_entry_bytes(&entry).expect("full entry serializes"),
        "canonical accounting must exclude JSON byte-array amplification"
    );
    assert_eq!(
        transcript_growth([&entry, &metadata_only]).projected_tokens,
        Some(
            projected
                + projected_transcript_entry_tokens(&metadata_only).expect("metadata projection")
        )
    );
}

/// Provider-irrelevant raw tool payloads must not inflate proactive compaction
/// projection when provider lowering sends only the normalized rendering.
#[test]
fn tool_result_projection_ignores_raw_payload_size() {
    let tiny_raw = tool_result_entry(1, 32);
    let large_raw = tool_result_entry(400_000, 32);

    assert_eq!(
        projected_transcript_entry_tokens(&tiny_raw),
        projected_transcript_entry_tokens(&large_raw)
    );
    assert!(
        projected_transcript_entry_tokens(&large_raw).expect("projection") < 1_000,
        "the provider-visible rendering, not structured consumer data, owns projection"
    );
}

/// Responses replay alternatives must contribute one large payload to provider
/// projection even though the durable transcript retains typed and raw copies.
#[test]
fn provider_replay_alternatives_charge_one_large_payload() {
    let payload = "z".repeat(170_000);
    let fixtures = [
        assistant_entry(ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: payload.clone(),
            }],
            phase: None,
            responses_raw_json: Some(format!(
                r#"{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{payload}"}}]}}"#
            )),
        })),
        assistant_entry(ContextItem::Compaction(OpaqueProviderItem::with_raw_json(
            CborValue::Map(vec![
                (
                    CborValue::Text("type".to_owned()),
                    CborValue::Text("compaction".to_owned()),
                ),
                (
                    CborValue::Text("encrypted_content".to_owned()),
                    CborValue::Text(payload.clone()),
                ),
            ]),
            format!(r#"{{"type":"compaction","encrypted_content":"{payload}"}}"#),
        ))),
        assistant_entry(ContextItem::ToolCall(ToolCallItem {
            call_id: "call-raw-arguments".into(),
            name: tau_proto::ToolName::new("audit_fixture"),
            tool_type: ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("payload".to_owned()),
                CborValue::Text(payload.clone()),
            )]),
            raw_arguments_json: Some(format!(r#"{{"payload":"{payload}"}}"#)),
            responses_envelope: None,
        })),
    ];

    for entry in fixtures {
        let durable = serialized_transcript_entry_bytes(&entry).expect("durable JSON");
        let projected = projected_transcript_entry_tokens(&entry).expect("projection");
        assert!(
            projected >= payload.len() as u64,
            "one provider-visible representation must remain charged"
        );
        assert!(
            durable >= projected + 150_000,
            "the duplicate durable sidecar must not enter provider projection: durable={durable}, projected={projected}"
        );
    }
}

/// Assistant projection must bound the hybrid item produced when provider
/// lowering keeps a raw envelope but rebases it with different typed text.
#[test]
fn assistant_replay_projection_charges_raw_envelope_and_rebased_text() {
    let typed_payload = "t".repeat(170_000);
    let envelope_payload = "e".repeat(170_000);
    let entry = assistant_entry(ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: typed_payload.clone(),
        }],
        phase: None,
        responses_raw_json: Some(format!(
            r#"{{"type":"message","role":"assistant","provider_owned":"{envelope_payload}","content":[{{"type":"output_text","text":"old"}}]}}"#
        )),
    }));

    let projected = projected_transcript_entry_tokens(&entry).expect("projection");
    assert!(
        projected >= (typed_payload.len() + envelope_payload.len()) as u64,
        "the bound must include retained raw fields plus rebased typed text: {projected}"
    );
}

/// A two-entry suffix with six duplicated tool payloads must stay below the
/// threshold while retaining a one-byte-per-provider-JSON-byte upper bound.
#[test]
fn duplicated_tool_payloads_do_not_schedule_tiny_context_compaction() {
    let assistant = user_entry(&"a".repeat(5_000));
    let results = AgentEntry::ToolResults {
        items: (0..6)
            .map(|index| {
                let AgentEntry::ToolResults { mut items } = tool_result_entry(37_000, 37_000)
                else {
                    unreachable!("fixture is a tool result")
                };
                items[0].call_id = format!("call-{index}").into();
                items.pop().expect("one result")
            })
            .collect(),
    };
    let growth = transcript_growth([&assistant, &results]);
    let exact = growth.serialized_bytes.expect("exact durable JSON");
    let projected = projected_input_tokens(Some(8_856), growth.projected_tokens, 4_096)
        .expect("checked projection");

    assert!(
        334_800 < exact,
        "the durable representation must reproduce the historical false trigger"
    );
    assert!(
        projected < 334_800,
        "provider-visible projection must not schedule the tiny-context compaction"
    );
}

/// Error, cancellation, and harness-authenticated presentation must use the
/// same complete provider text that prompt assembly and adapters lower.
#[test]
fn terminal_status_and_presentation_drive_tool_result_projection() {
    let mut error = tool_result_entry(400_000, 8);
    let AgentEntry::ToolResults { items } = &mut error else {
        unreachable!("fixture is a tool result")
    };
    items[0].status = ToolResultStatus::Error {
        message: "error\nmessage".repeat(20),
    };
    items[0].presentation = tau_proto::ToolResultPresentation::HarnessDedupPointer;
    let mut cancelled = error.clone();
    let AgentEntry::ToolResults { items } = &mut cancelled else {
        unreachable!("fixture is a tool result")
    };
    items[0].status = ToolResultStatus::Cancelled {
        reason: "cancelled\nreason".repeat(20),
    };

    for entry in [&error, &cancelled] {
        let projected = projected_transcript_entry_tokens(entry).expect("projection");
        assert!(
            projected < 2_000,
            "status text and framing must count without retaining raw payloads"
        );
    }
    assert_ne!(
        projected_transcript_entry_tokens(&error),
        projected_transcript_entry_tokens(&cancelled),
        "distinct provider terminal renderings must remain visible"
    );
    let ordinary_error = {
        let mut entry = error.clone();
        let AgentEntry::ToolResults { items } = &mut entry else {
            unreachable!("fixture is a tool result")
        };
        items[0].presentation = tau_proto::ToolResultPresentation::ToolPayload;
        entry
    };
    assert_ne!(
        projected_transcript_entry_tokens(&error),
        projected_transcript_entry_tokens(&ordinary_error),
        "authenticated pointer framing must change the projected provider text"
    );
}

/// An already-materialized compaction replacement must not reinterpret the
/// durable presentation discriminator or frame its provider text again.
#[test]
fn compaction_window_tool_result_uses_provider_projection() {
    let AgentEntry::ToolResults { mut items } = tool_result_entry(400_000, 32) else {
        unreachable!("fixture is a tool result")
    };
    items[0].presentation = tau_proto::ToolResultPresentation::HarnessDedupPointer;
    let compacted_pointer = AgentEntry::Compaction {
        replacement_window: items.into_iter().map(ContextItem::ToolResult).collect(),
        transaction_id: None,
        cut: None,
        suffix_end: None,
    };
    let mut compacted_payload = compacted_pointer.clone();
    let AgentEntry::Compaction {
        replacement_window, ..
    } = &mut compacted_payload
    else {
        unreachable!("fixture is a compaction")
    };
    let ContextItem::ToolResult(result) = &mut replacement_window[0] else {
        unreachable!("fixture contains one tool result")
    };
    result.presentation = tau_proto::ToolResultPresentation::ToolPayload;

    let pointer_projection =
        projected_transcript_entry_tokens(&compacted_pointer).expect("pointer projection");
    let payload_projection =
        projected_transcript_entry_tokens(&compacted_payload).expect("payload projection");
    assert!(pointer_projection < 1_000);
    assert_eq!(
        pointer_projection, payload_projection,
        "presentation metadata on materialized items must not reinterpret provider text"
    );
}

/// Removing raw structured payloads must not hide a genuinely large
/// provider-visible tool result from proactive compaction.
#[test]
fn large_provider_rendering_still_crosses_compaction_threshold() {
    let entry = tool_result_entry(1, 400_000);
    let projected = projected_input_tokens(
        Some(8_856),
        Some(projected_transcript_entry_tokens(&entry).expect("projection")),
        4_096,
    )
    .expect("checked projection");

    assert!(
        334_800 <= projected,
        "one-byte-per-provider-JSON-byte accounting must remain conservative"
    );
}

/// Projection requires both exact inputs and checked additions, including the
/// second addition of the reserve.
#[test]
fn transcript_projection_omits_unavailable_or_overflowed_derivations() {
    assert_eq!(projected_input_tokens(Some(100), None, 10), None);
    assert_eq!(projected_input_tokens(Some(u64::MAX), Some(1), 0), None);
    assert_eq!(projected_input_tokens(Some(u64::MAX - 1), Some(1), 1), None);
}
