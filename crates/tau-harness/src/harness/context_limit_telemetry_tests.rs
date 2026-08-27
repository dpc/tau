use tau_core::AgentEntry;
use tau_proto::{ContentPart, ContextItem, ContextLimitObservation, ContextRole, MessageItem};

use super::context_limit_telemetry::{
    context_limit_observation, serialized_transcript_entry_bytes, transcript_growth,
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

#[test]
fn provider_usage_classifies_rejection_against_advertised_limit() {
    assert_eq!(
        context_limit_observation(
            Some(tau_proto::TokenCount::new(127_000)),
            Some(tau_proto::TokenCount::new(128_000)),
        ),
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    );
    assert_eq!(
        context_limit_observation(
            Some(tau_proto::TokenCount::new(128_000)),
            Some(tau_proto::TokenCount::new(128_000)),
        ),
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    );
    for usage in [None, Some(tau_proto::TokenCount::ZERO)] {
        assert_eq!(
            context_limit_observation(usage, Some(tau_proto::TokenCount::new(128_000))),
            ContextLimitObservation::InsufficientEvidence
        );
    }
}

#[test]
fn transcript_growth_preserves_exact_json_utf8_and_escape_bytes() {
    let bytes = |text| {
        serialized_transcript_entry_bytes(&user_entry(text))
            .expect("serialize")
            .get()
    };
    assert_eq!(bytes("abcd") - bytes("a"), 3);
    assert_eq!(bytes("é") - bytes("a"), 1);
    assert_eq!(bytes("\"") - bytes("a"), 1);
    assert_eq!(bytes("\n") - bytes("a"), 1);

    let first = user_entry("first");
    let second = user_entry("second");
    let growth = transcript_growth([&first, &second])
        .serialized_bytes
        .expect("sum");
    assert_eq!(
        growth.get(),
        serialized_transcript_entry_bytes(&first)
            .expect("first")
            .get()
            + serialized_transcript_entry_bytes(&second)
                .expect("second")
                .get()
    );
}
