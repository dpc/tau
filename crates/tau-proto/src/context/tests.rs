use super::*;

/// Provider compaction items form canonical replacement context, while harness
/// triggers and private local envelopes must never become durable replacements.
#[test]
fn compaction_window_accepts_provider_item_and_rejects_harness_trigger() {
    let provider_item = ContextItem::Compaction(OpaqueProviderItem::with_raw_json(
        CborValue::Map(vec![]),
        r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#.to_owned(),
    ));

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
