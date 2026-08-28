use tau_proto::{
    ProviderQuotaBindingProvenance, ProviderQuotaLimitId, ProviderQuotaRouteBinding,
    ProviderQuotaWindowId,
};

use super::*;

const NOW: u64 = 2_000_000_000_000;
const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
const WEEK_TOLERANCE_SECONDS: u64 = WEEK_SECONDS * 5 / 100;
const SOFT_STALE_MS: u64 = 15 * 60 * 1_000;
const HARD_STALE_MS: u64 = 60 * 60 * 1_000;

/// Builds a trusted weekly sample so each test varies only the behavior it
/// intends to cover.
fn window(used: u16, elapsed_basis_points: u16) -> ProviderQuotaWindow {
    let remaining = WEEK_SECONDS.saturating_mul(u64::from(10_000 - elapsed_basis_points)) / 10_000;
    ProviderQuotaWindow {
        key: ProviderQuotaWindowKey {
            limit_id: ProviderQuotaLimitId::parse("codex").expect("valid quota test value"),
            window_id: ProviderQuotaWindowId::parse("secondary").expect("valid quota test value"),
        },
        used_basis_points: used,
        usage_observed_at_unix_ms: tau_proto::UnixMillis::new(NOW),
        window_seconds: tau_proto::QuotaWindowSeconds::new(WEEK_SECONDS),
        reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(NOW / 1_000 + remaining)),
        remaining_seconds_at_timing_anchor: Some(tau_proto::SignedSeconds::new(remaining as i64)),
        timing_anchor_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(NOW)),
        server_offset_ms: Some(tau_proto::ServerOffsetMillis::new(0)),
        server_offset_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(NOW)),
    }
}

/// Creates an explicitly bound snapshot; pool presence alone is intentionally
/// not sufficient in production or tests.
fn make_state(sample: ProviderQuotaWindow) -> (QuotaPacingState, ModelId) {
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let changed = HarnessProviderQuotaChanged {
        provider: model.provider.clone(),
        profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("valid quota test value"),
        sequence: tau_proto::ProviderQuotaSequence::new(2),
        windows: vec![sample],
        route_bindings: vec![ProviderQuotaRouteBinding {
            model: model.clone(),
            limit_ids: vec![ProviderQuotaLimitId::parse("codex").expect("valid quota test value")],
            observed_at_unix_ms: tau_proto::UnixMillis::new(NOW),
            provenance: ProviderQuotaBindingProvenance::TurnEvent,
        }],
    };
    let mut state = QuotaPacingState::default();
    state.update(&changed);
    (state, model)
}

/// Locks the approved exact entry thresholds and early-week far-under grace.
#[test]
fn exact_thresholds_and_grace_are_stable() {
    let (mut state, model) = make_state(window(0, 999));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Aligned)
    );
    let (mut state, model) = make_state(window(0, 1_500));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::FarUnder)
    );
    let (mut state, model) = make_state(window(6_000, 5_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
    let (mut state, model) = make_state(window(7_500, 5_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Danger)
    );
    let (mut state, model) = make_state(window(9_000, 9_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Danger)
    );
}

/// Verifies the approved three-point recovery hysteresis independently for
/// under, over, and both danger triggers.
#[test]
fn three_point_hysteresis_prevents_boundary_flicker() {
    let (mut state, model) = make_state(window(3_500, 5_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::FarUnder)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(3_700, 5_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::FarUnder)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(3_800, 5_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Aligned)
    );

    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(6_000, 5_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(5_800, 5_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(5_700, 5_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Aligned)
    );

    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(9_000, 7_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Danger)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(8_700, 7_000);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Danger)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows[0] = window(8_699, 6_499);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
}

/// Missing explicit applicability remains neutral when provider quota
/// current-state exists, without inferring a colored claim from a sole pool.
#[test]
fn sole_pool_never_implies_model_binding() {
    let (mut state, model) = make_state(window(5_000, 5_000));
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .bindings
        .clear();
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Unknown)
    );
}

/// Soft and hard staleness remain neutral while provider quota capability is
/// known rather than retaining a misleading color or hiding the status.
#[test]
fn binding_and_window_staleness_remain_neutral() {
    let (mut state, model) = make_state(window(5_000, 5_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW + SOFT_STALE_MS + 1)),
        Some(QuotaPacing::Unknown)
    );
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW + HARD_STALE_MS + 1)),
        Some(QuotaPacing::Unknown)
    );
}

/// A passed reset is unknown until an authoritative provider observation
/// establishes a new cycle; the UI must never synthesize a zero-use rollover.
#[test]
fn passed_reset_is_unknown_not_locally_reset() {
    let (mut state, model) = make_state(window(5_000, 9_990));
    let reset = state.current[&model.provider].windows[0]
        .reset_at_unix_seconds
        .expect("valid quota test value")
        .get()
        * 1_000;
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(reset)),
        Some(QuotaPacing::Unknown)
    );
}

/// Multiple explicitly bound pools use the worst weekly state only when every
/// member is present and trustworthy.
#[test]
fn all_of_bindings_are_conservative() {
    let (mut state, model) = make_state(window(3_500, 5_000));
    let mut second = window(7_500, 5_000);
    second.key.limit_id =
        ProviderQuotaLimitId::parse("codex_fast").expect("valid quota test value");
    let current = state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value");
    current.windows.push(second);
    current.bindings[0]
        .limit_ids
        .push(ProviderQuotaLimitId::parse("codex_fast").expect("valid quota test value"));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Danger)
    );
    state
        .current
        .get_mut(&model.provider)
        .expect("valid quota test value")
        .windows
        .pop();
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Unknown)
    );
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW + HARD_STALE_MS + 1)),
        Some(QuotaPacing::Unknown)
    );
}

/// Weekly selection follows the accepted ±5 percent duration tolerance and
/// ignores unrelated short windows.
#[test]
fn only_weekly_windows_participate() {
    let mut short = window(9_900, 5_000);
    short.window_seconds = tau_proto::QuotaWindowSeconds::new(5 * 60 * 60);
    let (mut state, model) = make_state(short);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Unknown)
    );
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW + SOFT_STALE_MS + 1)),
        Some(QuotaPacing::Unknown)
    );
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW + HARD_STALE_MS + 1)),
        Some(QuotaPacing::Unknown)
    );
    let mut edge = window(5_000, 5_000);
    edge.window_seconds = tau_proto::QuotaWindowSeconds::new(WEEK_SECONDS + WEEK_TOLERANCE_SECONDS);
    let (mut state, model) = make_state(edge);
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Aligned)
    );
}

/// Hard-expired timing evidence neutralizes an otherwise fresh usage sample.
#[test]
fn hard_expiry_includes_independent_timing_evidence() {
    let (mut state, model) = make_state(window(5_000, 5_000));
    let current = state
        .current
        .get_mut(&model.provider)
        .expect("provider quota state");
    current.windows[0].usage_observed_at_unix_ms = tau_proto::UnixMillis::new(NOW);
    current.windows[0].timing_anchor_observed_at_unix_ms =
        Some(tau_proto::UnixMillis::new(NOW - HARD_STALE_MS - 1));
    current.windows[0].server_offset_observed_at_unix_ms =
        Some(tau_proto::UnixMillis::new(NOW - HARD_STALE_MS - 1));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Unknown)
    );
}

/// Empty quota current-state is provider capability evidence and renders
/// unknown, while a provider that has published no state remains inapplicable.
#[test]
fn provider_capability_controls_unknown_visibility() {
    let model = ModelId::from("chatgpt/gpt-5.6-sol");
    let mut state = QuotaPacingState::default();
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        None
    );

    state.update(&HarnessProviderQuotaChanged {
        provider: model.provider.clone(),
        profile_epoch: ProviderQuotaEpoch::parse("epoch-empty").expect("quota epoch"),
        sequence: tau_proto::ProviderQuotaSequence::new(1),
        windows: Vec::new(),
        route_bindings: Vec::new(),
    });
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Unknown)
    );
    assert_eq!(
        state.classify(
            &ModelId::from("other/model"),
            tau_proto::UnixMillis::new(NOW)
        ),
        None
    );
}

/// Provider reset-anchor corrections within one minute retain hysteresis, while
/// a larger authoritative cycle change starts from the nominal classifier.
#[test]
fn reset_cycle_tolerance_controls_hysteresis_memory() {
    let (mut state, model) = make_state(window(6_000, 5_000));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
    let current = state
        .current
        .get_mut(&model.provider)
        .expect("provider quota state");
    current.windows[0] = window(5_800, 5_000);
    current.windows[0].reset_at_unix_seconds = current.windows[0]
        .reset_at_unix_seconds
        .map(|reset| tau_proto::UnixSeconds::new(reset.get() + 60));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Over)
    );
    let current = state
        .current
        .get_mut(&model.provider)
        .expect("provider quota state");
    current.windows[0].reset_at_unix_seconds = current.windows[0]
        .reset_at_unix_seconds
        .map(|reset| tau_proto::UnixSeconds::new(reset.get() + 61));
    assert_eq!(
        state.classify(&model, tau_proto::UnixMillis::new(NOW)),
        Some(QuotaPacing::Aligned)
    );
}
