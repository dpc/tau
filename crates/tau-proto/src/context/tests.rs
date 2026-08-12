use super::*;

/// Provider compaction items form canonical replacement context, while
/// harness compaction triggers must never become a durable replacement.
#[test]
fn compaction_window_accepts_provider_item_and_rejects_harness_trigger() {
    let provider_item = ContextItem::Compaction(OpaqueProviderItem::with_raw_json(
        CborValue::Map(vec![]),
        r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#.to_owned(),
    ));

    assert!(ValidatedCompactionWindow::new(vec![provider_item]).is_ok());
    assert!(validate_compaction_window(&[ContextItem::CompactionTrigger]).is_err());
}
