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
        lower_compat(&compat).cache_usage,
        tau_provider_chat_completions::CacheUsageCompat::DeepSeek
    );
    assert_eq!(
        lower_compat(&Default::default()).cache_usage,
        tau_provider_chat_completions::CacheUsageCompat::None
    );
}

/// An explicit route-level selector omission reaches request lowering without
/// being replaced by the ordinary Chat Completions default.
#[test]
fn tool_choice_capability_lowers_explicitly() {
    let compat: super::super::ChatCompletionsCompat =
        serde_json::from_value(serde_json::json!({"tool_choice": false}))
            .expect("selector compatibility");

    assert!(!lower_compat(&compat).tool_choice);
    assert!(lower_compat(&Default::default()).tool_choice);
}

/// Both structurally valid public cache variants must reach the matching
/// adapter mode without a fallback that could discard explicit ownership.
#[test]
fn public_cache_options_lower_by_variant() {
    let lower = |options| {
        let compat: super::super::ChatCompletionsCompat =
            serde_json::from_value(serde_json::json!({
                "openai_prompt_cache": {
                    "key": "agent",
                    "options": options
                }
            }))
            .expect("valid public cache compatibility");
        lower_compat(&compat)
            .prompt_cache
            .expect("lowered cache policy")
    };

    assert_eq!(
        lower(serde_json::json!({"mode": "implicit", "ttl": "30m"})),
        tau_provider_chat_completions::PromptCache {
            mode: tau_provider_chat_completions::PromptCacheMode::Implicit,
            ttl: tau_provider_chat_completions::PromptCacheTtl::Minutes30,
        }
    );
    assert_eq!(
        lower(serde_json::json!({
            "mode": "explicit",
            "ttl": "30m",
            "boundary": "system_prompt"
        })),
        tau_provider_chat_completions::PromptCache {
            mode: tau_provider_chat_completions::PromptCacheMode::Explicit,
            ttl: tau_provider_chat_completions::PromptCacheTtl::Minutes30,
        }
    );
}
use crate::chat_completions::LocalSummaryCompactionConfig;

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
    tiny.max_output_bytes = NonZeroU64::new(1);
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
        tau_proto::ContextItem::Reasoning(
            tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"reasoning"}"#)
                .expect("valid reasoning item"),
        ),
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

/// Assistant final text plus any attempted tool call must fail atomically, so
/// no narrative crosses the extension seam and the harness cannot execute it.
#[test]
fn narrative_output_rejects_text_mixed_with_attempted_tool_call() {
    let attempted_call = tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: "call-compaction-must-not-run".into(),
        name: tau_proto::ToolName::new("dangerous"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: Some(r#"{"path":"/must/not/run"}"#.to_owned()),
        responses_envelope: None,
    });
    assert!(
        validate_narrative_output(
            vec![valid_narrative_item(), attempted_call],
            narrative_config(4096),
        )
        .is_err()
    );
}

fn narrative_config(max_output_bytes: u64) -> LocalSummaryCompactionConfig {
    LocalSummaryCompactionConfig {
        max_input_bytes: NonZeroU64::new(4096),
        max_output_tokens: NonZeroU32::new(512),
        max_output_bytes: NonZeroU64::new(max_output_bytes),
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
