use super::ProviderTokenUsage;

/// A nonzero cache-read ceiling must survive the CBOR representation used
/// by protocol and persisted event payloads.
#[test]
fn provider_usage_round_trips_cache_read_ceiling() {
    let usage = ProviderTokenUsage {
        prompt_sent_tokens: 4_096,
        prompt_cached_tokens: 3_584,
        prompt_cache_read_ceiling_tokens: Some(3_584),
        ..Default::default()
    };
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(&usage, &mut cbor).expect("serialize usage CBOR");
    let decoded: ProviderTokenUsage =
        ciborium::de::from_reader(cbor.as_slice()).expect("deserialize usage CBOR");
    assert_eq!(decoded, usage);
}
