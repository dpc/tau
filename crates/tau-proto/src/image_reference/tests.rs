use crate::*;

/// Builds a reference with a recognizable synthetic digest, never real payload
/// authority.
fn reference() -> ToolResultContentPart {
    ToolResultContentPart::ImageReference(ImageReference {
        blob: ArtifactDescriptor::new(
            ArtifactKey::from_digest(&"ab".repeat(32)).expect("digest"),
            3,
        )
        .expect("descriptor"),
        media_type: ImageMediaType::Png,
        width: 2,
        height: 3,
        detail: ImageDetail::High,
    })
}

/// New CBOR references remain distinct from readable legacy inline images.
#[test]
fn reference_and_legacy_inline_round_trip_without_ambiguous_payloads() {
    let reference = reference();
    let encoded = encode_message_to_vec(&reference).expect("encode");
    assert_eq!(
        decode_message_from_slice::<ToolResultContentPart>(&encoded).expect("decode"),
        reference
    );
    let inline = serde_json::json!({
        "type": "image",
        "content": {"media_type":"png", "data":[1,2,3], "width":2, "height":3, "detail":"high"}
    });
    let decoded: ToolResultContentPart = serde_json::from_value(inline.clone()).expect("legacy");
    assert_eq!(
        decoded.inline_image().expect("inline").data.as_ref(),
        &[1, 2, 3]
    );
    let mut ambiguous_reference = serde_json::to_value(&reference).expect("reference");
    ambiguous_reference["content"]["data"] = serde_json::json!([1, 2, 3]);
    assert!(serde_json::from_value::<ToolResultContentPart>(ambiguous_reference).is_err());
    let mut ambiguous_inline = inline;
    ambiguous_inline["content"]["blob"] = serde_json::json!({"key":"unexpected", "size":3});
    assert!(serde_json::from_value::<ToolResultContentPart>(ambiguous_inline).is_err());
}

/// Old typed image references reject full public keys instead of silently
/// truncating stored verification identity during historical decode.
#[test]
fn historical_full_key_image_reference_is_rejected() {
    let mut historical = serde_json::to_value(reference()).expect("reference");
    historical["content"]["blob"]["key"] = serde_json::json!(format!("blake3:{}", "ab".repeat(32)));
    assert!(serde_json::from_value::<ToolResultContentPart>(historical.clone()).is_err());
    let encoded = encode_message_to_vec(&historical).expect("historical CBOR");
    assert!(decode_message_from_slice::<ToolResultContentPart>(&encoded).is_err());
}

/// Debug and generic projections must not disclose a key that permits fetching.
#[test]
fn debug_and_payload_clear_omit_fetchable_reference() {
    let mut part = reference();
    let ToolResultContentPart::ImageReference(image) = &part else {
        panic!("reference fixture")
    };
    let key = image.blob.key.clone();
    assert!(!format!("{part:?}").contains(key.as_str()));
    assert_eq!(part.encoded_image_bytes(), 3);
    part.clear_payload();
    assert!(!part.is_image_reference());
    let image = part.inline_image().expect("safe projection");
    assert_eq!((image.width, image.height), (2, 3));
    assert!(image.data.is_empty());
    assert!(
        !serde_json::to_string(&part)
            .expect("projection")
            .contains(key.as_str())
    );
}

/// Every typed context carrier detects and clears references without rewriting
/// opaque replay.
#[test]
fn context_reference_detection_covers_all_blocks_and_preserves_opaque_json() {
    let result = ToolResultItem {
        call_id: "image".into(),
        tool_type: ToolType::Function,
        status: ToolResultStatus::Success,
        output: ToolResponse::from_cbor(&CborValue::Null),
        presentation: Default::default(),
        provider_content: vec![reference()],
    };
    let opaque = OpaqueProviderItem::from_raw_json(
        r#"{"type":"reasoning","opaque":"image_reference blake3:opaque"}"#,
    )
    .expect("opaque");
    for block in [
        ContextBlock::ToolResults(ToolResultsBlock {
            items: vec![result.clone()],
        }),
        ContextBlock::UserInput(UserInputBlock {
            items: vec![ContextItem::ToolResult(result.clone())],
        }),
        ContextBlock::AssistantResponse(AssistantResponseBlock {
            provider_response_id: None,
            backend: None,
            usage: None,
            output_items: vec![ContextItem::ToolResult(result.clone())],
        }),
    ] {
        let mut context = PromptContext {
            blocks: vec![block],
        };
        assert!(context.has_image_references());
        context.clear_provider_image_bytes();
        assert!(!context.has_image_references());
        assert!(
            !serde_json::to_string(&context)
                .expect("context")
                .contains("abababab")
        );
    }
    let mut context = PromptContext {
        blocks: vec![ContextBlock::UserInput(UserInputBlock {
            items: vec![ContextItem::Reasoning(opaque)],
        })],
    };
    let before = context.clone();
    assert!(!context.has_image_references());
    context.clear_provider_image_bytes();
    assert_eq!(context, before);
}

/// Missing invocation policy never silently persists prepared images.
#[test]
fn prepared_image_storage_defaults_inline_and_round_trips_explicit_authority() {
    let legacy: ToolInvocationPolicy = serde_json::from_str("{}").expect("legacy policy");
    assert_eq!(legacy.prepared_image_storage, PreparedImageStorage::Inline);
    assert_eq!(serde_json::to_string(&legacy).expect("inline"), "{}");
    let policy = ToolInvocationPolicy {
        prepared_image_storage: PreparedImageStorage::Artifact,
        ..Default::default()
    };
    let encoded = encode_message_to_vec(&policy).expect("encode");
    assert_eq!(
        decode_message_from_slice::<ToolInvocationPolicy>(&encoded).expect("decode"),
        policy
    );
}
