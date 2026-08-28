use super::*;

/// The semantic sequence wrapper must retain the protocol's legacy scalar JSON
/// representation, including its valid zero and maximum values.
#[test]
fn provider_quota_sequence_preserves_scalar_json() {
    for value in [0, 1, u64::MAX] {
        let sequence = ProviderQuotaSequence::new(value);
        let encoded = serde_json::to_string(&sequence).expect("sequence serializes");
        assert_eq!(encoded, value.to_string());
        assert_eq!(
            serde_json::from_str::<ProviderQuotaSequence>(&encoded)
                .expect("legacy scalar sequence decodes"),
            sequence
        );
    }
}

/// The semantic sequence wrapper must encode exactly like the legacy scalar in
/// CBOR so existing protocol peers see no shape change at boundary values.
#[test]
fn provider_quota_sequence_preserves_scalar_cbor() {
    for value in [0, 1, u64::MAX] {
        let sequence = ProviderQuotaSequence::new(value);
        let mut encoded = Vec::new();
        ciborium::into_writer(&sequence, &mut encoded).expect("sequence serializes");
        let mut scalar_encoded = Vec::new();
        ciborium::into_writer(&value, &mut scalar_encoded).expect("scalar serializes");
        assert_eq!(encoded, scalar_encoded);
        assert_eq!(
            ciborium::from_reader::<ProviderQuotaSequence, _>(encoded.as_slice())
                .expect("legacy scalar sequence decodes"),
            sequence
        );
    }
}

/// Provider sequence exhaustion must keep the previous saturating producer
/// behavior rather than wrapping into an apparently fresh ordering cursor.
#[test]
fn provider_quota_sequence_advance_saturates() {
    assert_eq!(
        ProviderQuotaSequence::new(u64::MAX).saturating_next(),
        ProviderQuotaSequence::new(u64::MAX)
    );
}
