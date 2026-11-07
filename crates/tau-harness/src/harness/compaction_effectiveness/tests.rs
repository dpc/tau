use super::*;
use crate::harness::compaction_supplement;

/// Build ordinary text history with explicit retained role/framing.
fn text(text: &str) -> ContextItem {
    ContextItem::Message(tau_proto::MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

/// A large unchanged suffix must not masquerade as growth caused by compaction;
/// replacing two framed messages with one removes both content and one framing.
#[test]
fn fixed_recipe_and_suffix_preserve_the_same_reduction() {
    let prefix = vec![text(&"p".repeat(400)), text(&"p".repeat(400))];
    let suffix = vec![text(&"s".repeat(4000))];
    let replacement = vec![text(&"r".repeat(40))];
    let result = measure(Some(100), &prefix, &suffix, &replacement);
    assert_eq!(result.prefix_before, Some(TokenCount::new(208)));
    assert_eq!(result.replacement_after, Some(TokenCount::new(14)));
    assert_eq!(result.total_before, Some(TokenCount::new(1312)));
    assert_eq!(result.total_after, Some(TokenCount::new(1118)));
    let arrived = measure(
        Some(100),
        &prefix,
        &[suffix, vec![text("late")]].concat(),
        &replacement,
    );
    assert_eq!(
        arrived.total_before.expect("arrived before").get()
            - result.total_before.expect("before").get(),
        5
    );
    assert_eq!(
        arrived.total_after.expect("arrived after").get()
            - result.total_after.expect("after").get(),
        5
    );
}

/// Opaque replay uses audited normalization, not ciphertext/JSON tokenization.
#[test]
fn opaque_replacement_uses_versioned_normalization() {
    let opaque = tau_proto::OpaqueProviderItem::from_raw_json(&format!(
        r#"{{"type":"compaction","encrypted_content":"{}"}}"#,
        "x".repeat(1200)
    ))
    .expect("valid compaction");
    let result = measure(
        Some(10),
        &[text(&"p".repeat(4000))],
        &[],
        &[ContextItem::Compaction(opaque)],
    );
    assert_eq!(result.estimator, ESTIMATOR);
    assert_eq!(result.replacement_after, Some(TokenCount::new(104)));
    assert_eq!(result.total_after, Some(TokenCount::new(114)));
    let reasoning = tau_proto::OpaqueProviderItem::from_raw_json(&format!(
        r#"{{"type":"reasoning","encrypted_content":"{}"}}"#,
        "x".repeat(1200)
    ))
    .expect("valid reasoning");
    assert_eq!(item_tokens(&ContextItem::Reasoning(reasoning)), Some(67));
}

/// Local generation is measured only after the private narrative becomes the
/// installed synthetic user message, without charging its private envelope.
#[test]
fn local_summary_counts_the_installed_user_replacement() {
    let narrative =
        ContextItem::LocalCompactionNarrative(tau_proto::LocalCompactionNarrativeItem {
            narrative: "r".repeat(40),
        });
    let replacement = compaction_supplement::compose(&[narrative])
        .expect("accepted narrative")
        .expect("local summary");
    let result = measure(
        Some(10),
        &[text(&"p".repeat(400))],
        &[],
        replacement.items(),
    );
    assert_eq!(result.replacement_after, Some(TokenCount::new(14)));
    assert_eq!(result.total_after, Some(TokenCount::new(24)));
}

/// Unknown content and overflow affect only the dependent measurements, never
/// become zero, and do not erase a supported replacement contribution.
#[test]
fn unsupported_and_overflow_remain_unknown() {
    let unknown = ContextItem::UnknownProviderItem(
        tau_proto::OpaqueProviderItem::from_raw_json(r#"{"type":"future_media","data":"x"}"#)
            .expect("valid unknown item"),
    );
    let result = measure(Some(10), std::slice::from_ref(&unknown), &[], &[text("r")]);
    assert_eq!(result.prefix_before, None);
    assert_eq!(result.total_before, None);
    assert_eq!(result.total_after, Some(TokenCount::new(15)));
    let result = measure(Some(10), &[text("p")], &[unknown], &[text("r")]);
    assert_eq!(result.total_before, None);
    assert_eq!(result.total_after, None);
    assert_eq!(result.prefix_before, Some(TokenCount::new(5)));
    let result = measure(Some(u64::MAX), &[text("p")], &[], &[text("r")]);
    assert_eq!(result.total_before, None);
    assert_eq!(result.total_after, None);
    assert_eq!(
        measure(Some(0), &[], &[], &[]).total_before,
        Some(TokenCount::new(0))
    );
}
