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

/// New quota events retain their tagged JSON names and are transient state
/// without any field capable of carrying provider credentials.
#[test]
fn quota_event_round_trips_as_transient_wire_state() {
    let event = crate::Event::ProviderQuotaClear(ProviderQuotaClear {
        provider: ProviderName::new("chatgpt"),
        profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("valid quota test value"),
        sequence: 2,
    });
    let json = serde_json::to_string(&event).expect("valid quota test value");
    assert!(json.contains("\"event\":\"provider.quota_clear\""));
    assert_eq!(
        serde_json::from_str::<crate::Event>(&json).expect("valid quota test value"),
        event
    );
    assert!(event.defaults_to_transient());
}
