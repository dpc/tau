use super::ObservationId;

/// Protects the canonical textual representation without assigning ordering
/// or sequence semantics to the opaque random bytes.
#[test]
fn observation_id_uses_canonical_lowercase_hex() {
    let id = ObservationId::from_bytes([
        0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
        0x32,
    ]);
    let json = serde_json::to_string(&id).expect("serialize id");
    assert_eq!(json, "\"000123456789abcdeffedcba98765432\"");
    assert_eq!(
        serde_json::from_str::<ObservationId>(&json).expect("deserialize id"),
        id
    );
    assert!(serde_json::from_str::<ObservationId>("\"000123456789ABCDEFFEDCBA98765432\"").is_err());
}
