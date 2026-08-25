use super::*;

fn narrative(text: &str) -> Vec<ContextItem> {
    vec![ContextItem::LocalCompactionNarrative(
        tau_proto::LocalCompactionNarrativeItem {
            narrative: text.to_owned(),
        },
    )]
}

/// The accepted model final text must become exactly one synthetic user
/// checkpoint without wrappers, escaping, or deterministic supplements.
#[test]
fn final_text_becomes_exact_user_checkpoint() {
    let text = "exact <model> text & tool status stays model-owned";
    let window = compose(&narrative(text))
        .expect("valid envelope")
        .expect("local");
    assert_eq!(
        window.items(),
        [ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: text.to_owned()
            }],
            phase: None,
            responses_raw_json: None,
        })]
    );
}

/// Empty, oversized, and multi-item local envelopes must fail atomically.
#[test]
fn malformed_local_envelopes_fail() {
    assert!(compose(&narrative(" \n")).is_err());
    assert!(
        compose(&narrative(
            &"x".repeat(tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES + 1)
        ))
        .is_err()
    );
    let mut multiple = narrative("one");
    multiple.extend(narrative("two"));
    assert!(compose(&multiple).is_err());
}

/// Provider-native replacement windows must bypass local checkpoint conversion.
#[test]
fn provider_native_window_is_unchanged() {
    let native = vec![ContextItem::Compaction(tau_proto::OpaqueProviderItem {
        value: tau_proto::CborValue::Null,
        raw_json: None,
    })];
    assert!(matches!(compose(&native), Ok(None)));
}
