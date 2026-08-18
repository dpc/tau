//! Summary compactor output validation regression tests.

use std::num::{NonZeroU32, NonZeroU64};

use super::*;

/// The serialized cache usage capability reaches the finite backend attempt
/// without URL- or model-based inference.
#[test]
fn cache_usage_capability_lowers_explicitly() {
    let compat: super::super::ChatCompletionsCompat =
        serde_json::from_value(serde_json::json!({"cache_usage": "deep_seek"}))
            .expect("declared DeepSeek compatibility");

    assert_eq!(
        lower_compat(compat).cache_usage,
        tau_provider_chat_completions::CacheUsageCompat::DeepSeek
    );
    assert_eq!(
        lower_compat(Default::default()).cache_usage,
        tau_provider_chat_completions::CacheUsageCompat::None
    );
}
use crate::chat_completions::{
    LocalSummaryCompactionConfig, LocalSummaryCompactionSerializationProfile,
};

/// Ensures a bounded free-form narrative becomes one private harness envelope
/// while empty and oversized messages fail.
#[test]
fn narrative_output_is_bounded_and_lowered_to_private_envelope() {
    let config = narrative_config(4096);
    let expected_narrative = "The current task is ready for a later agent.\nA useful fact follows.";
    let item = tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: expected_narrative.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });

    let accepted = validate_narrative_output(vec![item.clone()], config).expect("valid narrative");
    let tau_proto::ContextItem::LocalCompactionNarrative(envelope) = accepted else {
        panic!("narrative must lower to a private narrative envelope");
    };
    assert_eq!(envelope.narrative, expected_narrative);

    let mut tiny = config;
    tiny.max_output_bytes = NonZeroU64::new(1).expect("positive");
    assert!(validate_narrative_output(vec![item], tiny).is_err());

    let empty = tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: " \n\t".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    assert!(validate_narrative_output(vec![empty], config).is_err());
    let wrong_role = tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::User,
        content: vec![tau_proto::ContentPart::Text {
            text: narrative_text().to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    assert!(validate_narrative_output(vec![wrong_role], config).is_err());
    assert!(
        validate_narrative_output(vec![tau_proto::ContextItem::CompactionTrigger], config).is_err()
    );
    assert!(
        validate_narrative_output(vec![valid_narrative_item(), valid_narrative_item()], config)
            .is_err()
    );
}

/// Ensures local-summary reasoning is independently bounded and discarded
/// rather than entering the durable synthetic checkpoint.
#[test]
fn narrative_output_discards_separately_bounded_reasoning() {
    let items = vec![
        reasoning_item("private"),
        valid_narrative_item(),
        reasoning_item(" thought"),
    ];

    let accepted = validate_narrative_output(items, narrative_config(128))
        .expect("bounded reasoning and narrative");
    let encoded = serde_json::to_string(&accepted).expect("private narrative envelope");
    assert!(!encoded.contains("private"));
    assert!(!encoded.contains("thought"));

    assert!(
        validate_narrative_output(
            vec![
                reasoning_item(&"x".repeat(64)),
                valid_narrative_item(),
                reasoning_item(&"x".repeat(65)),
            ],
            narrative_config(128),
        )
        .is_err()
    );
}

/// Ensures reasoning never substitutes for the one required assistant message
/// and every non-reasoning side item remains fail-closed.
#[test]
fn narrative_output_requires_one_assistant_message_and_rejects_other_items() {
    let config = narrative_config(4096);
    assert!(validate_narrative_output(vec![reasoning_item("only reasoning")], config).is_err());
    assert!(
        validate_narrative_output(
            vec![
                reasoning_item("before"),
                valid_narrative_item(),
                valid_narrative_item(),
            ],
            config,
        )
        .is_err()
    );
    for unsupported in [
        tau_proto::ContextItem::CompactionTrigger,
        tau_proto::ContextItem::Reasoning(tau_proto::OpaqueProviderItem::new(
            tau_proto::CborValue::Null,
        )),
    ] {
        assert!(
            validate_narrative_output(
                vec![
                    reasoning_item("before"),
                    valid_narrative_item(),
                    unsupported
                ],
                config,
            )
            .is_err()
        );
    }
}

fn narrative_config(max_output_bytes: u64) -> LocalSummaryCompactionConfig {
    LocalSummaryCompactionConfig {
        serialization_profile: LocalSummaryCompactionSerializationProfile::LocalTranscriptV1,
        context_window_tokens: NonZeroU64::new(8192).expect("positive"),
        max_input_bytes: NonZeroU64::new(4096).expect("positive"),
        max_output_tokens: NonZeroU32::new(512).expect("positive"),
        max_output_bytes: NonZeroU64::new(max_output_bytes).expect("positive"),
    }
}

fn narrative_text() -> &'static str {
    "Goal:\ngoal\nConstraints:\nnone\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact"
}

fn valid_narrative_item() -> tau_proto::ContextItem {
    tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: narrative_text().to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

fn reasoning_item(text: &str) -> tau_proto::ContextItem {
    tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: text.to_owned(),
    })
}
