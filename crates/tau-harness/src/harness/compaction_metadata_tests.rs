use super::*;

#[test]
fn compacted_estimate_uses_items_from_latest_compaction_forward() {
    // Regression coverage for compaction UI metadata: provider replay keeps
    // the latest compaction item and every later item, so fallback sizing
    // must not estimate only the opaque compaction item itself.
    let compaction =
        ContextItem::Compaction(tau_proto::OpaqueProviderItem::new(CborValue::Map(vec![(
            CborValue::Text("type".to_owned()),
            CborValue::Text("compaction".to_owned()),
        )])));
    let after = ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: "large post-compaction assistant item".repeat(20),
        }],
        phase: None,
    });
    let items = vec![
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: "pre-compaction item".to_owned(),
            }],
            phase: None,
        }),
        compaction.clone(),
        after,
    ];

    let replay_window = latest_compaction_replay_window(&items).expect("compaction window");
    assert_eq!(replay_window.len(), 2);
    assert!(
        estimate_compacted_input_tokens(replay_window).expect("replay window has token estimate")
            > estimate_compacted_input_tokens(&[compaction])
                .expect("compaction event has token estimate")
    );
}

/// Ensures fallback compaction metadata estimates the provider-visible raw
/// opaque item sidecar when replay would send it instead of the parsed CBOR
/// projection.
#[test]
fn compacted_estimate_uses_raw_opaque_provider_item_size_when_available() {
    let compact = ContextItem::Compaction(tau_proto::OpaqueProviderItem::with_raw_json(
        CborValue::Map(vec![(
            CborValue::Text("type".to_owned()),
            CborValue::Text("compaction".to_owned()),
        )]),
        format!(
            r#"{{"type":"compaction","padding":"{}"}}"#,
            "x".repeat(10_000)
        ),
    ));

    let tokens = estimate_compacted_input_tokens(&[compact]).expect("raw estimate");

    assert!(
        tokens >= 2_000,
        "raw sidecar should dominate the compact token estimate, got {tokens}"
    );
}
