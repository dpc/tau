use std::num::NonZeroU64;

use super::*;

type ContractMutation = fn(&mut serde_json::Value);

fn valid_contract() -> ProviderCacheContract {
    ProviderCacheContract {
        kind: tau_proto::ProviderCacheKind::AutomaticPrefix,
        ttl: tau_proto::ProviderCacheTtl::SlidingKnown {
            seconds: NonZeroU64::new(300).expect("positive test duration"),
        },
        renewal: tau_proto::ProviderCacheRenewal::Read,
        output_floor: tau_proto::ProviderCacheOutputFloor::Zero,
        quota: tau_proto::ProviderCacheQuotaAccounting {
            requests: tau_proto::ProviderCacheQuotaCharge::CountsFully,
            read_tokens: tau_proto::ProviderCacheQuotaCharge::Exempt,
            write_tokens: tau_proto::ProviderCacheQuotaCharge::CountsFully,
            output_tokens: tau_proto::ProviderCacheQuotaCharge::Exempt,
        },
        privacy: tau_proto::ProviderCachePrivacy {
            storage: tau_proto::ProviderCacheStorageMode::VolatileMemory,
            zero_data_retention: tau_proto::ProviderCacheZeroDataRetentionCompatibility::Compatible,
            data_residency: tau_proto::ProviderCacheDataResidencyEffect::PreservesRoutePolicy,
            manual_deletion: tau_proto::ProviderCacheDeletionAvailability::Unavailable,
        },
    }
}

/// Proves generic profile declarations publish adapter-owned prefix identity
/// metadata without accepting a user-supplied cache key or version.
#[test]
fn configured_contract_lowers_to_runtime_policy() {
    let contract = valid_contract();
    let value = serde_json::to_value(contract).expect("serialize contract");
    assert!(value.get("prefix_identity_version").is_none());

    let policy = contract.runtime_policy().expect("valid runtime policy");
    assert_eq!(policy.prefix_identity_version.get(), 1);
    assert_eq!(policy.ttl, contract.ttl);
}

/// Proves programmatic construction cannot bypass the same cross-field
/// invariants enforced during profile decoding.
#[test]
fn direct_construction_is_revalidated_before_publication() {
    let mut contract = valid_contract();
    contract.ttl = tau_proto::ProviderCacheTtl::Minimum {
        seconds: NonZeroU64::new(300).expect("positive test duration"),
    };

    assert!(contract.runtime_policy().is_err());
}

/// Proves a read-renewal claim cannot turn a minimum lifetime into a sliding
/// hard deadline.
#[test]
fn configured_contract_rejects_read_without_sliding_ttl() {
    let mut value = serde_json::to_value(valid_contract()).expect("serialize contract");
    value["ttl"] = serde_json::json!({"kind": "minimum", "seconds": 300});

    let error = serde_json::from_value::<ProviderCacheContract>(value)
        .expect_err("minimum lifetime must reject read renewal");
    assert!(error.to_string().contains("requires ttl `sliding_known`"));
}

/// Proves generic production profiles cannot claim named-object deletion when
/// no current backend owns a typed delete operation.
#[test]
fn configured_contract_rejects_manual_deletion_support() {
    let mut value = serde_json::to_value(valid_contract()).expect("serialize contract");
    value["privacy"]["manual_deletion"] = serde_json::json!("supported");

    let error = serde_json::from_value::<ProviderCacheContract>(value)
        .expect_err("unsupported deletion capability must fail");
    assert!(error.to_string().contains("manual deletion is unsupported"));
}

/// Proves a generic profile can truthfully publish externally managed
/// Gemini-style object metadata without claiming that Tau creates, patches, or
/// deletes that object.
#[test]
fn configured_contract_accepts_external_explicit_object_policy() {
    let contract: ProviderCacheContract = serde_json::from_value(serde_json::json!({
        "kind": "explicit_object",
        "ttl": {"kind": "fixed", "seconds": 3600},
        "renewal": "patch_expiry",
        "output_floor": "zero",
        "quota": {
            "requests": "unknown",
            "read_tokens": "unknown",
            "write_tokens": "unknown",
            "output_tokens": "unknown"
        },
        "privacy": {
            "storage": "named_provider_object",
            "zero_data_retention": "incompatible",
            "data_residency": "provider_specific",
            "manual_deletion": "unavailable"
        }
    }))
    .expect("externally managed explicit cache object");

    let policy = contract
        .runtime_policy()
        .expect("valid explicit object policy");
    assert!(matches!(
        policy.ttl,
        tau_proto::ProviderCacheTtl::Fixed { seconds } if seconds.get() == 3_600
    ));
    assert_eq!(
        (policy.kind, policy.renewal, policy.privacy.storage),
        (
            tau_proto::ProviderCacheKind::ExplicitObject,
            tau_proto::ProviderCacheRenewal::PatchExpiry,
            tau_proto::ProviderCacheStorageMode::NamedProviderObject,
        )
    );
    assert_eq!(
        policy.privacy.manual_deletion,
        tau_proto::ProviderCacheDeletionAvailability::Unavailable
    );
}

/// Proves every provider-independent object/renewal rejection and strict nested
/// config boundary fails closed before model publication.
#[test]
fn configured_contract_rejects_invalid_nested_shapes() {
    let valid = serde_json::to_value(valid_contract()).expect("serialize contract");
    let cases: &[(&str, ContractMutation)] = &[
        ("patch expiry without object", |value| {
            value["renewal"] = serde_json::json!("patch_expiry");
            value["ttl"] = serde_json::json!({"kind": "fixed", "seconds": 300});
        }),
        ("patch expiry without fixed ttl", |value| {
            value["kind"] = serde_json::json!("explicit_object");
            value["renewal"] = serde_json::json!("patch_expiry");
        }),
        ("named storage without object", |value| {
            value["privacy"]["storage"] = serde_json::json!("named_provider_object");
        }),
        ("zero ttl", |value| {
            value["ttl"] = serde_json::json!({"kind": "sliding_known", "seconds": 0});
        }),
        ("user-supplied prefix identity version", |value| {
            value["prefix_identity_version"] = serde_json::json!(1);
        }),
        ("unknown ttl member", |value| {
            value["ttl"]["secondz"] = serde_json::json!(300);
        }),
        ("unknown quota member", |value| {
            value["quota"]["requestz"] = serde_json::json!("counts_fully");
        }),
        ("unknown privacy member", |value| {
            value["privacy"]["region"] = serde_json::json!("somewhere");
        }),
    ];

    for (name, mutate) in cases {
        let mut value = valid.clone();
        mutate(&mut value);
        assert!(
            serde_json::from_value::<ProviderCacheContract>(value).is_err(),
            "{name} unexpectedly decoded"
        );
    }
}
