use std::sync::Arc;

use tau_core::AgentEntry;
use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextLimitObservation, ContextRole, ImageContent,
    ImageDetail, ImageMediaType, MessageItem, ToolResultContentPart, ToolResultItem,
    ToolResultStatus, ToolType,
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
        inference_activation: true,
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

/// Projection requires both exact inputs and checked additions, including the
/// second addition of the reserve.
#[test]
fn transcript_projection_omits_unavailable_or_overflowed_derivations() {
    assert_eq!(projected_input_tokens(Some(100), None, 10), None);
    assert_eq!(projected_input_tokens(Some(u64::MAX), Some(1), 0), None);
    assert_eq!(projected_input_tokens(Some(u64::MAX - 1), Some(1), 1), None);
}
