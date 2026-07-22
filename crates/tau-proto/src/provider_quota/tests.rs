use super::*;

/// Quota identifiers reject unbounded or provider-controlled display prose at
/// deserialization, before those values become map keys.
#[test]
fn quota_identifiers_are_bounded_ascii_keys() {
    assert!(ProviderQuotaLimitId::parse("codex_fast").is_ok());
    assert!(ProviderQuotaLimitId::parse("Codex Fast").is_err());
    assert!(ProviderQuotaLimitId::parse("x".repeat(MAX_PROVIDER_QUOTA_ID_LEN + 1)).is_err());
    assert!(serde_json::from_str::<ProviderQuotaLimitId>("\"../secret\"").is_err());
}

/// Full-state validation enforces uniqueness, percentages, paired timing
/// dimensions, exact provider ownership, and all-of cardinality.
#[test]
fn state_validation_rejects_ambiguous_or_untrusted_shapes() {
    let provider = ProviderName::new("chatgpt");
    let window = ProviderQuotaWindow {
        key: ProviderQuotaWindowKey {
            limit_id: ProviderQuotaLimitId::parse("codex").expect("valid quota test value"),
            window_id: ProviderQuotaWindowId::parse("secondary").expect("valid quota test value"),
        },
        used_basis_points: 5_000,
        usage_observed_at_unix_ms: 1,
        window_seconds: 604_800,
        reset_at_unix_seconds: Some(700_000),
        remaining_seconds_at_timing_anchor: Some(300_000),
        timing_anchor_observed_at_unix_ms: Some(1),
        server_offset_ms: Some(0),
        server_offset_observed_at_unix_ms: Some(1),
    };
    let binding = ProviderQuotaRouteBinding {
        model: ModelId::from("chatgpt/gpt-5.6-sol"),
        limit_ids: vec![ProviderQuotaLimitId::parse("codex").expect("valid quota test value")],
        observed_at_unix_ms: 1,
        provenance: ProviderQuotaBindingProvenance::TurnEvent,
    };
    assert!(
        validate_provider_quota_state(
            &provider,
            std::slice::from_ref(&window),
            std::slice::from_ref(&binding)
        )
        .is_ok()
    );
    assert!(
        validate_provider_quota_state(
            &provider,
            &[window.clone(), window.clone()],
            std::slice::from_ref(&binding)
        )
        .is_err()
    );
    let mut invalid_percent = window.clone();
    invalid_percent.used_basis_points = 10_001;
    assert!(
        validate_provider_quota_state(
            &provider,
            &[invalid_percent],
            std::slice::from_ref(&binding)
        )
        .is_err()
    );
    let mut mismatched = binding;
    mismatched.model = ModelId::from("other/model");
    assert!(validate_provider_quota_state(&provider, &[window], &[mismatched]).is_err());
}

/// Every quota report retains its exact JSON/EventName tag, survives CBOR, and
/// defaults to transient publication without credential-bearing fields.
#[test]
fn quota_report_family_round_trips_as_transient_wire_state() {
    let provider = ProviderName::new("chatgpt");
    let epoch = ProviderQuotaEpoch::parse("epoch-1").expect("valid quota test value");
    for (event, tag, name) in [
        (
            crate::Event::ProviderQuotaReplaceReported(ProviderQuotaReplace {
                provider: provider.clone(),
                profile_epoch: epoch.clone(),
                sequence: 1,
                establishes_new_epoch: true,
                windows: Vec::new(),
                route_bindings: Vec::new(),
            }),
            "provider.quota_replace_reported",
            crate::EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
        ),
        (
            crate::Event::ProviderQuotaPatchReported(ProviderQuotaPatch {
                provider: provider.clone(),
                profile_epoch: epoch.clone(),
                sequence: 2,
                windows: Vec::new(),
                removed_window_keys: Vec::new(),
                route_bindings: Vec::new(),
            }),
            "provider.quota_patch_reported",
            crate::EventName::PROVIDER_QUOTA_PATCH_REPORTED,
        ),
        (
            crate::Event::ProviderQuotaClearReported(ProviderQuotaClear {
                provider: provider.clone(),
                profile_epoch: epoch.clone(),
                sequence: 3,
            }),
            "provider.quota_clear_reported",
            crate::EventName::PROVIDER_QUOTA_CLEAR_REPORTED,
        ),
    ] {
        let json = serde_json::to_string(&event).expect("encode quota report as JSON");
        assert!(json.contains(&format!("\"event\":\"{tag}\"")));
        assert_eq!(
            serde_json::from_str::<crate::Event>(&json).expect("decode quota report from JSON"),
            event
        );
        let mut cbor = Vec::new();
        ciborium::into_writer(&event, &mut cbor).expect("encode quota report as CBOR");
        assert_eq!(
            ciborium::from_reader::<crate::Event, _>(cbor.as_slice())
                .expect("decode quota report from CBOR"),
            event
        );
        assert_eq!(event.name(), name);
        assert!(!event.defaults_to_persist());
    }
}
