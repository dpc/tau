//! Generic validation and pacing classification for provider quota snapshots.
//!
//! The boundary consumes only provider-neutral protocol state.  ChatGPT wire
//! formats and account credentials intentionally remain below it.

use std::collections::{HashMap, HashSet};

use tau_proto::{
    HarnessProviderQuotaChanged, ModelId, ProviderName, ProviderQuotaEpoch, ProviderQuotaWindow,
    ProviderQuotaWindowKey,
};

const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
const WEEK_TOLERANCE_SECONDS: u64 = WEEK_SECONDS * 5 / 100;
const CLOCK_TOLERANCE_MS: i128 = 5 * 60 * 1_000;
const SOFT_STALE_MS: u64 = 15 * 60 * 1_000;
const HARD_STALE_MS: u64 = 60 * 60 * 1_000;

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
    cycle: Option<u64>,
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
    pub(crate) fn classify(&mut self, model: &ModelId, now_unix_ms: u64) -> Option<QuotaPacing> {
        let current = self.current.get(&model.provider)?.clone();
        let Some(binding) = current
            .bindings
            .iter()
            .find(|binding| binding.model == *model)
        else {
            return Some(QuotaPacing::Unknown);
        };
        let binding_age = match age_ms(binding.observed_at_unix_ms, now_unix_ms) {
            Some(age) => age,
            None => return Some(QuotaPacing::Unknown),
        };
        if binding_age > HARD_STALE_MS {
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
        if missing_pool || binding_age > SOFT_STALE_MS {
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

fn window_hard_expired(window: &ProviderQuotaWindow, now_unix_ms: u64) -> bool {
    if age_ms(window.usage_observed_at_unix_ms, now_unix_ms).is_some_and(|age| age > HARD_STALE_MS)
    {
        return true;
    }
    let relative_expired = window
        .timing_anchor_observed_at_unix_ms
        .and_then(|observed| age_ms(observed, now_unix_ms))
        .is_some_and(|age| age > HARD_STALE_MS);
    let offset_expired = window
        .server_offset_observed_at_unix_ms
        .and_then(|observed| age_ms(observed, now_unix_ms))
        .is_some_and(|age| age > HARD_STALE_MS);
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

fn valid_fractions(window: &ProviderQuotaWindow, now_unix_ms: u64) -> Option<(f64, f64)> {
    let usage_age = age_ms(window.usage_observed_at_unix_ms, now_unix_ms)?;
    if usage_age > SOFT_STALE_MS {
        return None;
    }
    let duration_ms = i128::from(window.window_seconds).checked_mul(1_000)?;
    let relative_remaining_ms = match (
        window.remaining_seconds_at_timing_anchor,
        window.timing_anchor_observed_at_unix_ms,
    ) {
        (Some(remaining), Some(anchor)) => (|| {
            let timing_age = i128::from(now_unix_ms) - i128::from(anchor);
            if !(-CLOCK_TOLERANCE_MS..=i128::from(SOFT_STALE_MS)).contains(&timing_age) {
                return None;
            }
            i128::from(remaining)
                .checked_mul(1_000)?
                .checked_sub(timing_age)
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
            if age_ms(observed, now_unix_ms)? > SOFT_STALE_MS {
                None
            } else {
                Some(
                    i128::from(reset)
                        .checked_mul(1_000)?
                        .checked_sub(i128::from(now_unix_ms).checked_add(i128::from(offset))?)?,
                )
            }
        }
        _ => None,
    };
    if let (Some(relative), Some(absolute)) = (relative_remaining_ms, absolute_remaining_ms)
        && (relative - absolute).abs() > CLOCK_TOLERANCE_MS
    {
        return None;
    }
    let remaining_ms = relative_remaining_ms.or(absolute_remaining_ms)?;
    if remaining_ms < -CLOCK_TOLERANCE_MS
        || remaining_ms > duration_ms.checked_add(CLOCK_TOLERANCE_MS)?
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

fn age_ms(observed_at: u64, now: u64) -> Option<u64> {
    if observed_at > now.saturating_add(CLOCK_TOLERANCE_MS as u64) {
        return None;
    }
    Some(now.saturating_sub(observed_at))
}

fn is_weekly(seconds: u64) -> bool {
    seconds.abs_diff(WEEK_SECONDS) <= WEEK_TOLERANCE_SECONDS
}

fn same_reset_cycle(left: Option<u64>, right: Option<u64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.abs_diff(right) <= 60,
        (None, None) => true,
        _ => false,
    }
}

fn classify_window(used: f64, elapsed: f64, previous: Option<QuotaPacing>) -> QuotaPacing {
    let used = (used * 10_000.0).round() as i32;
    let elapsed = (elapsed * 10_000.0).round() as i32;
    let deviation = used - elapsed;
    match previous {
        Some(QuotaPacing::Danger) if deviation > 2_200 || used >= 8_700 => QuotaPacing::Danger,
        Some(QuotaPacing::Over) if deviation > 700 => {
            if used >= 9_000 || deviation >= 2_500 {
                QuotaPacing::Danger
            } else {
                QuotaPacing::Over
            }
        }
        Some(QuotaPacing::FarUnder) if deviation < -1_200 => QuotaPacing::FarUnder,
        _ if used >= 9_000 || deviation >= 2_500 => QuotaPacing::Danger,
        _ if deviation >= 1_000 => QuotaPacing::Over,
        _ if elapsed >= 1_000 && deviation <= -1_500 => QuotaPacing::FarUnder,
        _ => QuotaPacing::Aligned,
    }
}

#[cfg(test)]
#[path = "provider_quota/tests.rs"]
mod tests;
