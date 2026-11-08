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

/// Every bin boundary, exhaustion, and oversized input must stay a visible
/// single terminal cell, with more remaining quota never producing less height.
#[test]
fn remaining_quota_glyph_bins_are_clamped_and_one_cell() {
    for (remaining, expected) in [
        (0, "▁"),
        (1, "▁"),
        (1_250, "▁"),
        (1_251, "▂"),
        (2_500, "▂"),
        (2_501, "▃"),
        (3_750, "▃"),
        (3_751, "▄"),
        (5_000, "▄"),
        (5_001, "▅"),
        (6_250, "▅"),
        (6_251, "▆"),
        (7_500, "▆"),
        (7_501, "▇"),
        (8_750, "▇"),
        (8_751, "█"),
        (10_000, "█"),
        (u16::MAX, "█"),
    ] {
        let status = QuotaStatus {
            pacing: QuotaPacing::Aligned,
            remaining_basis_points: Some(remaining),
        };
        assert_eq!(status.glyph(), expected, "{remaining}");
        assert_eq!(unicode_width::UnicodeWidthStr::width(status.glyph()), 1);
    }
}

/// Presentation must use validated quota data and preserve schedule colors
/// while fitting alongside identity in the smallest useful status line.
#[test]
fn quota_status_height_color_unknown_and_narrow_layout() {
    use tau_cli_term::{PriorityLine, PriorityLineAlignment, PriorityLinePriority, Span};
    use tau_themes::names;

    for (used, elapsed, glyph, style) in [
        (0, 5_000, "█", names::STATUS_QUOTA_UNDER),
        (5_000, 5_000, "▄", names::STATUS_QUOTA_ALIGNED),
        (6_000, 5_000, "▄", names::STATUS_QUOTA_OVER),
        (10_000, 5_000, "▁", names::STATUS_QUOTA_DANGER),
        (u16::MAX, 5_000, "▁", names::STATUS_QUOTA_DANGER),
    ] {
        let (mut state, model) = make_state(window(used, elapsed));
        let status = state
            .status(&model, UnixMillis::new(NOW))
            .expect("capability");
        assert_eq!(status.glyph(), glyph);
        assert_eq!(status.style(), style);
        let theme = crate::tests::cli_test_theme();
        let mut line = PriorityLine::new();
        line.push(
            PriorityLinePriority::new(0),
            PriorityLineAlignment::Left,
            "A",
        );
        line.push(
            PriorityLinePriority::new(50),
            PriorityLineAlignment::Right,
            Span::new(
                status.glyph(),
                tau_cli_term::resolve::resolve(&theme, status.style()),
            ),
        );
        let text = |width| {
            line.layout(width)
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
        };
        assert_eq!(text(3), format!("A {glyph}"));
        assert_eq!(text(2), "A ");
        assert_eq!(text(1), "A");
        assert_eq!(text(0), "");
        let unknown = state
            .status(&model, UnixMillis::new(NOW + SOFT_STALE_MS + 1))
            .expect("capability");
        assert_eq!(unknown.glyph(), "?");
        assert_eq!(unknown.style(), names::STATUS_QUOTA_UNKNOWN);
    }
}

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
        state
            .status(&model, UnixMillis::new(NOW))
            .expect("capability")
            .glyph(),
        "▂"
    );
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
