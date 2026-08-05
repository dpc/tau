use super::ProviderTokenUsage;
use crate::ProviderCacheUsage;

/// Cache normalization clamps contradictory token classes without inventing
/// missing observations and derives a privacy-safe hit ratio.
#[test]
fn cache_usage_normalization_and_hit_ratio_are_deterministic() {
    let normalized = ProviderCacheUsage {
        read_tokens: Some(80),
        write_tokens: Some(50),
        miss_tokens: Some(50),
        cacheable_prefix_tokens: Some(200),
        avoided_prefill_tokens: Some(150),
        ..ProviderCacheUsage::default()
    }
    .normalized(100);

    assert_eq!(normalized.read_tokens, Some(80));
    assert_eq!(normalized.write_tokens, Some(20));
    assert_eq!(normalized.miss_tokens, Some(0));
    assert_eq!(normalized.cacheable_prefix_tokens, Some(100));
    assert_eq!(normalized.avoided_prefill_tokens, Some(100));
    assert_eq!(normalized.hit_ratio_millionths(), Some(1_000_000));
}

/// JSON and CBOR preserve an explicit zero cache observation while keeping an
/// absent cache record and absent members absent.
#[test]
fn cache_usage_wire_distinguishes_explicit_zero_from_absence() {
    let explicit = ProviderTokenUsage {
        cache: Some(Box::new(ProviderCacheUsage {
            read_tokens: Some(0),
            ..ProviderCacheUsage::default()
        })),
        ..ProviderTokenUsage::default()
    };
    let absent = ProviderTokenUsage::default();

    let explicit_json = serde_json::to_value(&explicit).expect("serialize explicit zero");
    let absent_json = serde_json::to_value(&absent).expect("serialize absent cache");
    assert_eq!(explicit_json["cache"]["read_tokens"], 0);
    assert!(explicit_json["cache"].get("write_tokens").is_none());
    assert!(absent_json.get("cache").is_none());

    for usage in [explicit, absent] {
        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&usage, &mut cbor).expect("serialize usage CBOR");
        let decoded: ProviderTokenUsage =
            ciborium::de::from_reader(cbor.as_slice()).expect("deserialize usage CBOR");
        assert_eq!(decoded, usage);
    }
}

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
