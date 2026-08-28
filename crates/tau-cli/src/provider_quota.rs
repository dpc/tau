//! Generic validation and pacing classification for provider quota snapshots.
//!
//! The boundary consumes only provider-neutral protocol state.  ChatGPT wire
//! formats and account credentials intentionally remain below it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tau_proto::{
    HarnessProviderQuotaChanged, ModelId, ProviderName, ProviderQuotaEpoch, ProviderQuotaWindow,
    ProviderQuotaWindowKey, QuotaWindowSeconds, SignedSeconds, UnixMillis, UnixSeconds,
};

const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const WEEK_TOLERANCE: Duration = Duration::from_secs(7 * 24 * 60 * 60 * 5 / 100);
const CLOCK_TOLERANCE: Duration = Duration::from_secs(5 * 60);
const SOFT_STALE: Duration = Duration::from_secs(15 * 60);
const HARD_STALE: Duration = Duration::from_secs(60 * 60);

/// User-visible quota pacing classification.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum QuotaPacing {
    /// Usage is at least 15 percentage points behind elapsed time.
    FarUnder,
    /// Usage is within the normal pacing band.
    Aligned,
    /// Usage is materially ahead of elapsed time.
    Over,
    /// Usage is far ahead or at least 90 percent exhausted.
    Danger,
    /// Provider quota capability exists but applicability, freshness, or timing
    /// is not trustworthy.
    Unknown,
}

impl QuotaPacing {
    /// Compact accessible text that remains meaningful without color.
    pub(crate) const fn chip(self) -> &'static str {
        match self {
            Self::FarUnder => "Q-",
            Self::Aligned => "Q=",
            Self::Over => "Q+",
            Self::Danger => "Q!",
            Self::Unknown => "Q?",
        }
    }
}

#[derive(Clone)]
struct CurrentQuota {
    profile_epoch: ProviderQuotaEpoch,
    windows: Vec<ProviderQuotaWindow>,
    bindings: Vec<tau_proto::ProviderQuotaRouteBinding>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct HysteresisKey {
    provider: ProviderName,
    epoch: ProviderQuotaEpoch,
    model: ModelId,
    window: ProviderQuotaWindowKey,
    cycle: Option<UnixSeconds>,
}

/// UI-local provider-current-state cache and Schmitt-trigger memory.
#[derive(Default)]
pub(crate) struct QuotaPacingState {
    current: HashMap<ProviderName, CurrentQuota>,
    hysteresis: HashMap<HysteresisKey, QuotaPacing>,
}

impl QuotaPacingState {
    /// Replaces one provider's harness-validated current state without rebasing
    /// any provider observation timestamps.
    pub(crate) fn update(&mut self, changed: &HarnessProviderQuotaChanged) {
        let provider = changed.provider.clone();
        let new_epoch = changed.profile_epoch.clone();
        if self
            .current
            .get(&provider)
            .is_some_and(|current| current.profile_epoch != new_epoch)
        {
            self.hysteresis.retain(|key, _| key.provider != provider);
        }
        self.current.insert(
            provider,
            CurrentQuota {
                profile_epoch: new_epoch,
                windows: changed.windows.clone(),
                bindings: changed.route_bindings.clone(),
            },
        );
    }

    /// Returns the current compact state for an exact selected model.
    ///
    /// `None` means the selected model's provider has published no quota
    /// current-state capability. Once that capability is known, incomplete,
    /// unbound, stale, expired, or timing-untrusted state is neutral unknown.
    pub(crate) fn classify(
        &mut self,
        model: &ModelId,
        now_unix_ms: UnixMillis,
    ) -> Option<QuotaPacing> {
        let current = self.current.get(&model.provider)?.clone();
        let Some(binding) = current
            .bindings
            .iter()
            .find(|binding| binding.model == *model)
        else {
            return Some(QuotaPacing::Unknown);
        };
        let binding_age = match age(binding.observed_at_unix_ms, now_unix_ms) {
            Some(age) => age,
            None => return Some(QuotaPacing::Unknown),
        };
        if HARD_STALE < binding_age {
            return Some(QuotaPacing::Unknown);
        }

        let bound: HashSet<_> = binding.limit_ids.iter().collect();
        let present: HashSet<_> = current
            .windows
            .iter()
            .map(|window| &window.key.limit_id)
            .collect();
        let missing_pool = !bound.iter().all(|limit| present.contains(*limit));

        let weekly = current
            .windows
            .iter()
            .filter(|window| {
                bound.contains(&window.key.limit_id) && is_weekly(window.window_seconds)
            })
            .cloned()
            .collect::<Vec<_>>();
        if weekly.is_empty() && !missing_pool {
            return Some(QuotaPacing::Unknown);
        }
        if weekly
            .iter()
            .any(|window| window_hard_expired(window, now_unix_ms))
        {
            return Some(QuotaPacing::Unknown);
        }
        if missing_pool || SOFT_STALE < binding_age {
            return Some(QuotaPacing::Unknown);
        }

        let mut aggregate = QuotaPacing::FarUnder;
        for window in weekly {
            let Some((used, elapsed)) = valid_fractions(&window, now_unix_ms) else {
                return Some(QuotaPacing::Unknown);
            };
            let key = HysteresisKey {
                provider: model.provider.clone(),
                epoch: current.profile_epoch.clone(),
                model: model.clone(),
                window: window.key.clone(),
                cycle: window.reset_at_unix_seconds,
            };
            let prior_key = self
                .hysteresis
                .keys()
                .find(|candidate| {
                    candidate.provider == key.provider
                        && candidate.epoch == key.epoch
                        && candidate.model == key.model
                        && candidate.window == key.window
                        && same_reset_cycle(candidate.cycle, key.cycle)
                })
                .cloned();
            let previous = prior_key
                .as_ref()
                .and_then(|prior| self.hysteresis.remove(prior));
            let pacing = classify_window(used, elapsed, previous);
            self.hysteresis.insert(key, pacing);
            aggregate = aggregate.max(pacing);
        }
        Some(aggregate)
    }
}

fn window_hard_expired(window: &ProviderQuotaWindow, now_unix_ms: UnixMillis) -> bool {
    if age(window.usage_observed_at_unix_ms, now_unix_ms).is_some_and(|age| HARD_STALE < age) {
        return true;
    }
    let relative_expired = window
        .timing_anchor_observed_at_unix_ms
        .and_then(|observed| age(observed, now_unix_ms))
        .is_some_and(|age| HARD_STALE < age);
    let offset_expired = window
        .server_offset_observed_at_unix_ms
        .and_then(|observed| age(observed, now_unix_ms))
        .is_some_and(|age| HARD_STALE < age);
    match (
        window.timing_anchor_observed_at_unix_ms,
        window.server_offset_observed_at_unix_ms,
    ) {
        (Some(_), Some(_)) => relative_expired && offset_expired,
        (Some(_), None) => relative_expired,
        (None, Some(_)) => offset_expired,
        (None, None) => false,
    }
}

fn valid_fractions(window: &ProviderQuotaWindow, now_unix_ms: UnixMillis) -> Option<(f64, f64)> {
    let usage_age = age(window.usage_observed_at_unix_ms, now_unix_ms)?;
    if SOFT_STALE < usage_age {
        return None;
    }
    let duration_ms = duration_millis(Duration::from_secs(window.window_seconds.get()))?;
    let relative_remaining_ms = match (
        window.remaining_seconds_at_timing_anchor,
        window.timing_anchor_observed_at_unix_ms,
    ) {
        (Some(remaining), Some(anchor)) => (|| {
            let timing_age = signed_milliseconds_between(anchor, now_unix_ms);
            if !(-duration_millis(CLOCK_TOLERANCE)?..=duration_millis(SOFT_STALE)?)
                .contains(&timing_age)
            {
                return None;
            }
            signed_seconds_to_millis(remaining)?.checked_sub(timing_age)
        })(),
        (None, None) => None,
        _ => None,
    };
    let absolute_remaining_ms = match (
        window.reset_at_unix_seconds,
        window.server_offset_ms,
        window.server_offset_observed_at_unix_ms,
    ) {
        (Some(reset), Some(offset), Some(observed)) => {
            if age(observed, now_unix_ms)? > SOFT_STALE {
                None
            } else {
                Some(unix_seconds_to_millis(reset)?.checked_sub(
                    i128::from(now_unix_ms.get()).checked_add(i128::from(offset.get()))?,
                )?)
            }
        }
        _ => None,
    };
    if let (Some(relative), Some(absolute)) = (relative_remaining_ms, absolute_remaining_ms)
        && (relative - absolute).abs() > duration_millis(CLOCK_TOLERANCE)?
    {
        return None;
    }
    let remaining_ms = relative_remaining_ms.or(absolute_remaining_ms)?;
    if remaining_ms < -duration_millis(CLOCK_TOLERANCE)?
        || remaining_ms > duration_ms.checked_add(duration_millis(CLOCK_TOLERANCE)?)?
    {
        return None;
    }
    // At reset the provider must establish a fresh cycle; never synthesize zero
    // usage locally.
    if remaining_ms <= 0 {
        return None;
    }
    let elapsed = (1.0 - remaining_ms as f64 / duration_ms as f64).clamp(0.0, 1.0);
    let used = f64::from(window.used_basis_points) / 10_000.0;
    Some((used, elapsed))
}

/// Returns elapsed wall-clock duration, rejecting observations too far in the
/// future.
fn age(observed_at: UnixMillis, now: UnixMillis) -> Option<Duration> {
    let tolerance = u64::try_from(CLOCK_TOLERANCE.as_millis()).expect("clock tolerance fits u64");
    if observed_at.get() > now.get().saturating_add(tolerance) {
        return None;
    }
    Some(Duration::from_millis(
        now.get().saturating_sub(observed_at.get()),
    ))
}

/// Converts a `Duration` to signed milliseconds for quota timing arithmetic.
fn duration_millis(duration: Duration) -> Option<i128> {
    duration.as_millis().try_into().ok()
}

/// Projects a Unix-seconds timestamp onto the millisecond arithmetic scale.
fn unix_seconds_to_millis(seconds: UnixSeconds) -> Option<i128> {
    i128::from(seconds.get()).checked_mul(1_000)
}

/// Projects a signed provider duration onto the millisecond arithmetic scale.
fn signed_seconds_to_millis(seconds: SignedSeconds) -> Option<i128> {
    i128::from(seconds.get()).checked_mul(1_000)
}

/// Computes the signed millisecond difference between two Unix-millisecond
/// timestamps.
fn signed_milliseconds_between(earlier: UnixMillis, later: UnixMillis) -> i128 {
    i128::from(later.get()) - i128::from(earlier.get())
}

/// Returns whether a provider-declared duration is within the weekly pacing
/// band.
fn is_weekly(seconds: QuotaWindowSeconds) -> bool {
    Duration::from_secs(seconds.get()).abs_diff(WEEK) <= WEEK_TOLERANCE
}

fn same_reset_cycle(left: Option<UnixSeconds>, right: Option<UnixSeconds>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.get().abs_diff(right.get()) <= 60,
        (None, None) => true,
        _ => false,
    }
}

fn classify_window(used: f64, elapsed: f64, previous: Option<QuotaPacing>) -> QuotaPacing {
    let used = (used * 10_000.0).round() as i32;
    let elapsed = (elapsed * 10_000.0).round() as i32;
    let deviation = used - elapsed;
    match previous {
        Some(QuotaPacing::Danger) if 2_200 < deviation || 8_700 <= used => QuotaPacing::Danger,
        Some(QuotaPacing::Over) if 700 < deviation => {
            if 9_000 <= used || 2_500 <= deviation {
                QuotaPacing::Danger
            } else {
                QuotaPacing::Over
            }
        }
        Some(QuotaPacing::FarUnder) if deviation < -1_200 => QuotaPacing::FarUnder,
        _ if 9_000 <= used || 2_500 <= deviation => QuotaPacing::Danger,
        _ if 1_000 <= deviation => QuotaPacing::Over,
        _ if 1_000 <= elapsed && deviation <= -1_500 => QuotaPacing::FarUnder,
        _ => QuotaPacing::Aligned,
    }
}

#[cfg(test)]
#[path = "provider_quota/tests.rs"]
mod tests;
