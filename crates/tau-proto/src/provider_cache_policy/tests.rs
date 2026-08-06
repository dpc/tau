use std::num::{NonZeroU32, NonZeroU64};

use super::*;
use crate::{
    ProviderCacheDataResidencyEffect, ProviderCacheDeletionAvailability, ProviderCacheQuotaCharge,
    ProviderCacheStorageMode, ProviderCacheZeroDataRetentionCompatibility,
};

/// Proves the runtime contract has a stable content-free JSON shape and keeps a
/// minimum lifetime distinct from a hard expiry.
#[test]
fn cache_policy_json_preserves_minimum_and_privacy_semantics() {
    let policy = ProviderCachePolicy {
        kind: ProviderCacheKind::ExplicitBreakpoint,
        ttl: ProviderCacheTtl::Minimum {
            seconds: NonZeroU64::new(1_800).expect("positive test duration"),
        },
        renewal: ProviderCacheRenewal::Recreate,
        output_floor: ProviderCacheOutputFloor::UnboundedReasoning,
        quota: ProviderCacheQuotaAccounting {
            requests: ProviderCacheQuotaCharge::CountsFully,
            read_tokens: ProviderCacheQuotaCharge::CountsFully,
            write_tokens: ProviderCacheQuotaCharge::CountsFully,
            output_tokens: ProviderCacheQuotaCharge::ProviderSpecific,
        },
        prefix_identity_version: NonZeroU32::new(1).expect("positive test version"),
        privacy: ProviderCachePrivacy {
            storage: ProviderCacheStorageMode::ExtendedProviderRetention,
            zero_data_retention: ProviderCacheZeroDataRetentionCompatibility::Incompatible,
            data_residency: ProviderCacheDataResidencyEffect::ProviderSpecific,
            manual_deletion: ProviderCacheDeletionAvailability::Unavailable,
        },
    };

    let value = serde_json::to_value(policy).expect("serialize cache policy");
    assert_eq!(
        value["ttl"],
        serde_json::json!({"kind": "minimum", "seconds": 1800})
    );
    assert_eq!(value["prefix_identity_version"], 1);
    assert_eq!(value["privacy"]["manual_deletion"], "unavailable");
    assert!(value.get("cache_key").is_none());
    assert!(value.get("object_id").is_none());
    assert_eq!(
        serde_json::from_value::<ProviderCachePolicy>(value).expect("round trip"),
        policy
    );
}

/// Proves every TTL class remains discrete on the wire, preventing unknown or
/// minimum observations from decoding as a documented hard deadline.
#[test]
fn cache_ttl_variants_round_trip_without_inference() {
    let variants = [
        ProviderCacheTtl::SlidingKnown {
            seconds: NonZeroU64::new(300).expect("positive test duration"),
        },
        ProviderCacheTtl::Minimum {
            seconds: NonZeroU64::new(1_800).expect("positive test duration"),
        },
        ProviderCacheTtl::Fixed {
            seconds: NonZeroU64::new(3_600).expect("positive test duration"),
        },
        ProviderCacheTtl::Unknown,
    ];

    for ttl in variants {
        let encoded = serde_json::to_vec(&ttl).expect("serialize ttl");
        assert_eq!(
            serde_json::from_slice::<ProviderCacheTtl>(&encoded).expect("decode ttl"),
            ttl
        );
    }
}
