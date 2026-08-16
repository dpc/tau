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

/// Ensures accepted summary text becomes a user-role, explicitly untrusted
/// historical checkpoint while malformed schemas and oversized text fail.
#[test]
fn summary_output_is_bounded_and_lowered_to_historical_context() {
    let config = summary_config(4096);
    let summary = [
        "Goal:",
        "goal",
        "Constraints:",
        "none",
        "Decisions:",
        "one",
        "Progress:",
        "done",
        "Open Work:",
        "next",
        "Critical Facts:",
        "fact",
    ]
    .join("\n");
    let item = tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text { text: summary }],
        phase: None,
        responses_raw_json: None,
    });

    let accepted = validate_summary_output(vec![item.clone()], config).expect("valid summary");
    let tau_proto::ContextItem::Message(message) = accepted else {
        panic!("summary must lower to a message");
    };
    assert_eq!(message.role, tau_proto::ContextRole::User);
    let encoded = serde_json::to_string(&message).expect("summary message");
    assert!(encoded.contains("untrusted synthetic historical checkpoint data"));

    let mut tiny = config;
    tiny.max_output_bytes = NonZeroU64::new(1).expect("positive");
    assert!(validate_summary_output(vec![item], tiny).is_err());

    for malformed in [
        "",
        "preamble\nGoal:\ngoal\nConstraints:\nnone\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact",
        "Goal:\ngoal\nConstraints:\n\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact",
        "Constraints:\nnone\nGoal:\ngoal\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact",
        " Goal:\ngoal\nConstraints:\nnone\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact",
    ] {
        let malformed = tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: malformed.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        });
        assert!(validate_summary_output(vec![malformed], config).is_err());
    }
    let wrong_role = tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::User,
        content: vec![tau_proto::ContentPart::Text {
            text: summary_text().to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    assert!(validate_summary_output(vec![wrong_role], config).is_err());
    assert!(
        validate_summary_output(vec![tau_proto::ContextItem::CompactionTrigger], config).is_err()
    );
    assert!(
        validate_summary_output(vec![valid_summary_item(), valid_summary_item()], config).is_err()
    );
}

/// Ensures local-summary reasoning is independently bounded and discarded
/// rather than entering the durable synthetic checkpoint.
#[test]
fn summary_output_discards_separately_bounded_reasoning() {
    let items = vec![
        reasoning_item("private"),
        valid_summary_item(),
        reasoning_item(" thought"),
    ];

    let accepted =
        validate_summary_output(items, summary_config(128)).expect("bounded reasoning and summary");
    let encoded = serde_json::to_string(&accepted).expect("summary message");
    assert!(!encoded.contains("private"));
    assert!(!encoded.contains("thought"));

    assert!(
        validate_summary_output(
            vec![
                reasoning_item(&"x".repeat(64)),
                valid_summary_item(),
                reasoning_item(&"x".repeat(65)),
            ],
            summary_config(128),
        )
        .is_err()
    );
}

/// Ensures reasoning never substitutes for the one required assistant message
/// and every non-reasoning side item remains fail-closed.
#[test]
fn summary_output_requires_one_assistant_message_and_rejects_other_items() {
    let config = summary_config(4096);
    assert!(validate_summary_output(vec![reasoning_item("only reasoning")], config).is_err());
    assert!(
        validate_summary_output(
            vec![
                reasoning_item("before"),
                valid_summary_item(),
                valid_summary_item(),
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
            validate_summary_output(
                vec![reasoning_item("before"), valid_summary_item(), unsupported],
                config,
            )
            .is_err()
        );
    }
}

fn summary_config(max_output_bytes: u64) -> LocalSummaryCompactionConfig {
    LocalSummaryCompactionConfig {
        serialization_profile: LocalSummaryCompactionSerializationProfile::LocalTranscriptV1,
        context_window_tokens: NonZeroU64::new(8192).expect("positive"),
        max_input_bytes: NonZeroU64::new(4096).expect("positive"),
        max_output_tokens: NonZeroU32::new(512).expect("positive"),
        max_output_bytes: NonZeroU64::new(max_output_bytes).expect("positive"),
    }
}

fn summary_text() -> &'static str {
    "Goal:\ngoal\nConstraints:\nnone\nDecisions:\none\nProgress:\ndone\nOpen Work:\nnext\nCritical Facts:\nfact"
}

fn valid_summary_item() -> tau_proto::ContextItem {
    tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: summary_text().to_owned(),
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
