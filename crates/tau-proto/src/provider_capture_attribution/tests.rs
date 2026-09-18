use super::*;

/// Operation IDs must stay canonical, private in Debug and distinct from
/// prompt authority, so malformed wire identifiers cannot reach filename code.
/// Debug must also omit the opaque payload's ordinary numeric byte
/// representation.
#[test]
fn operation_attribution_is_strict_private_and_class_limited() {
    let id = CacheOperationId::from_bytes([0xab; 16]);
    let text = id.to_hex();
    let zstd = b"PRIVATE_OPAQUE_BYTES".to_vec();
    let zstd_debug = format!("{zstd:?}");
    assert_eq!(CacheOperationId::parse(&text), Some(id));
    assert!(CacheOperationId::parse(&text.to_uppercase()).is_none());
    assert!(CacheOperationId::parse("../not-an-operation").is_none());
    assert!(!format!("{id:?}").contains(&text));
    let attribution = ProviderCaptureAttribution::CacheOperation(id);
    assert!(attribution.permits(crate::ProviderDebugCaptureClass::CacheDiagnostic));
    assert!(!attribution.permits(crate::ProviderDebugCaptureClass::WebsocketRequest));
    let message = crate::HarnessInputMessage::ProviderDebugCapture(crate::ProviderDebugCapture {
        session_id: crate::SessionId::parse("operation-session").expect("session"),
        attribution,
        class: crate::ProviderDebugCaptureClass::CacheDiagnostic,
        zstd,
    });
    let encoded = crate::encode_harness_input_to_vec(&message).expect("encode");
    assert_eq!(
        crate::decode_harness_input_from_slice(&encoded).expect("decode"),
        message
    );
    let debug = format!("{message:?}");
    assert!(!debug.contains(&text));
    assert!(!debug.contains("PRIVATE_OPAQUE_BYTES"));
    assert!(!debug.contains(&zstd_debug));
}
