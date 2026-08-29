use super::*;

/// Provider compaction items form canonical replacement context, while harness
/// triggers and private local envelopes must never become durable replacements.
#[test]
fn compaction_window_accepts_provider_item_and_rejects_harness_trigger() {
    let provider_item = ContextItem::Compaction(
        OpaqueProviderItem::from_raw_json(
            r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#,
        )
        .expect("valid opaque compaction"),
    );

    assert!(ValidatedCompactionWindow::new(vec![provider_item]).is_ok());
    assert!(validate_compaction_window(&[ContextItem::CompactionTrigger]).is_err());
    assert!(
        validate_compaction_window(&[ContextItem::LocalCompactionNarrative(
            LocalCompactionNarrativeItem {
                narrative: "private".to_owned(),
            },
        )])
        .is_err()
    );
}

/// Provider message phases retain only the two supported OpenAI wire values.
#[test]
fn message_phases_have_only_provider_supported_wire_values() {
    assert_eq!(MessagePhase::Commentary.as_openai_wire(), "commentary");
    assert_eq!(MessagePhase::FinalAnswer.as_openai_wire(), "final_answer");
}

/// Synthetic compaction-summary origin must survive protocol serialization
/// independently of the exact narrative bytes.
#[test]
fn synthetic_compaction_summary_origin_round_trips() {
    let part = ContentPart::SyntheticCompactionSummary {
        text: "<summary>& exact bytes".to_owned(),
    };
    let encoded = serde_json::to_value(&part).expect("serialize typed origin");
    assert_eq!(
        encoded,
        serde_json::json!({
            "type": "synthetic_compaction_summary",
            "text": "<summary>& exact bytes",
        })
    );
    assert_eq!(
        serde_json::from_value::<ContentPart>(encoded).expect("deserialize typed origin"),
        part
    );
}
