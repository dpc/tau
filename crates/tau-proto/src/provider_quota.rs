//! Bounded, provider-neutral account-quota state.
//!
//! Quota telemetry is transient current state.  It is deliberately separate
//! from per-response token usage and provider retry policy.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ModelId, ProviderName};

/// Maximum number of quota windows accepted in one provider snapshot.
pub const MAX_PROVIDER_QUOTA_WINDOWS: usize = 32;
/// Maximum number of model-to-pool bindings accepted in one provider snapshot.
pub const MAX_PROVIDER_QUOTA_BINDINGS: usize = 64;
/// Maximum number of simultaneously applicable pools in one route binding.
pub const MAX_PROVIDER_QUOTA_BINDING_LIMITS: usize = 8;
/// Maximum byte length of a quota epoch, pool id, or window id.
pub const MAX_PROVIDER_QUOTA_ID_LEN: usize = 64;

macro_rules! quota_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Validates and constructs this bounded quota identifier.
            pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
                let value = value.into();
                if value.is_empty() {
                    return Err("quota identifier must not be empty");
                }
                if value.len() > MAX_PROVIDER_QUOTA_ID_LEN {
                    return Err("quota identifier is too long");
                }
                if !value.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_' | b'.')
                }) {
                    return Err("quota identifier contains invalid characters");
                }
                Ok(Self(value))
            }

            /// Returns the validated wire identifier.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

quota_id!(
    ProviderQuotaEpoch,
    "Opaque generation identifying one provider profile/account lifetime."
);
quota_id!(
    ProviderQuotaLimitId,
    "Stable provider-normalized identifier for one quota pool."
);
quota_id!(
    ProviderQuotaWindowId,
    "Stable provider-normalized identifier for one window within a pool."
);

/// Stable compound key for one provider quota window.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaWindowKey {
    /// Provider-normalized quota-pool identifier.
    pub limit_id: ProviderQuotaLimitId,
    /// Provider-normalized stable window identifier.
    pub window_id: ProviderQuotaWindowId,
}

/// One complete normalized quota-window record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaWindow {
    /// Stable compound identity for this window.
    pub key: ProviderQuotaWindowKey,
    /// Used quota in basis points, where 10,000 means 100 percent.
    pub used_basis_points: u16,
    /// Unix milliseconds when usage was observed from the provider.
    pub usage_observed_at_unix_ms: u64,
    /// Server-declared window duration in seconds.
    pub window_seconds: u64,
    /// Server-declared absolute reset time, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_unix_seconds: Option<u64>,
    /// Remaining seconds at the independent timing anchor, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_seconds_at_timing_anchor: Option<i64>,
    /// Unix milliseconds at which the relative timing value was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_anchor_observed_at_unix_ms: Option<u64>,
    /// Calibrated provider-server offset in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_offset_ms: Option<i64>,
    /// Unix milliseconds when the server offset was calibrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_offset_observed_at_unix_ms: Option<u64>,
}

/// Provenance proving that a model route uses an exact quota pool set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderQuotaBindingProvenance {
    /// A successful provider turn explicitly identified its metered pool.
    TurnEvent,
    /// A provider response explicitly named the active limiting pool.
    ActiveLimitHeader,
}

/// Exact model-to-quota-pool applicability observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaRouteBinding {
    /// Exact provider-qualified model whose route was observed.
    pub model: ModelId,
    /// Complete all-of set of quota pools applying to this route.
    pub limit_ids: Vec<ProviderQuotaLimitId>,
    /// Unix milliseconds when applicability was explicitly observed.
    pub observed_at_unix_ms: u64,
    /// Provider evidence establishing this binding.
    pub provenance: ProviderQuotaBindingProvenance,
}

/// Atomic full provider quota snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaReplace {
    /// Provider namespace owning the state.
    pub provider: ProviderName,
    /// Opaque profile/account generation.
    pub profile_epoch: ProviderQuotaEpoch,
    /// Strictly increasing sequence within the epoch.
    pub sequence: u64,
    /// Whether this event establishes a previously unseen epoch.
    pub establishes_new_epoch: bool,
    /// Complete bounded set of current windows.
    pub windows: Vec<ProviderQuotaWindow>,
    /// Complete bounded set of current exact route bindings.
    pub route_bindings: Vec<ProviderQuotaRouteBinding>,
}

/// Sparse provider quota update containing complete records for changed keys.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaPatch {
    /// Provider namespace owning the state.
    pub provider: ProviderName,
    /// Opaque profile/account generation.
    pub profile_epoch: ProviderQuotaEpoch,
    /// Strictly increasing sequence within the epoch.
    pub sequence: u64,
    /// Complete records to upsert by stable key.
    pub windows: Vec<ProviderQuotaWindow>,
    /// Stable keys to remove.
    pub removed_window_keys: Vec<ProviderQuotaWindowKey>,
    /// Complete model bindings to replace by exact model id.
    pub route_bindings: Vec<ProviderQuotaRouteBinding>,
}

/// Explicit removal of quota state for one provider epoch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaClear {
    /// Provider namespace owning the state.
    pub provider: ProviderName,
    /// Epoch removed by this clear.
    pub profile_epoch: ProviderQuotaEpoch,
    /// Strictly increasing sequence within the cleared epoch.
    pub sequence: u64,
}

/// Harness-validated full current quota state for UI consumers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProviderQuotaChanged {
    /// Provider namespace owning the state.
    pub provider: ProviderName,
    /// Opaque profile/account generation.
    pub profile_epoch: ProviderQuotaEpoch,
    /// Latest accepted sequence for this epoch.
    pub sequence: u64,
    /// Complete validated current windows.
    pub windows: Vec<ProviderQuotaWindow>,
    /// Complete validated current route bindings.
    pub route_bindings: Vec<ProviderQuotaRouteBinding>,
}

/// Validates numeric, cardinality, uniqueness, and model/provider invariants.
pub fn validate_provider_quota_state(
    provider: &ProviderName,
    windows: &[ProviderQuotaWindow],
    bindings: &[ProviderQuotaRouteBinding],
) -> Result<(), &'static str> {
    if windows.len() > MAX_PROVIDER_QUOTA_WINDOWS {
        return Err("too many provider quota windows");
    }
    if bindings.len() > MAX_PROVIDER_QUOTA_BINDINGS {
        return Err("too many provider quota bindings");
    }
    let mut window_keys = std::collections::HashSet::new();
    for window in windows {
        if window.used_basis_points > 10_000 {
            return Err("provider quota usage exceeds 100 percent");
        }
        if window.window_seconds == 0 {
            return Err("provider quota duration must be positive");
        }
        if !window_keys.insert(&window.key) {
            return Err("duplicate provider quota window key");
        }
        if window.remaining_seconds_at_timing_anchor.is_some()
            != window.timing_anchor_observed_at_unix_ms.is_some()
        {
            return Err("provider quota relative timing fields must be paired");
        }
        if window.server_offset_ms.is_some() != window.server_offset_observed_at_unix_ms.is_some() {
            return Err("provider quota server offset fields must be paired");
        }
    }
    let mut models = std::collections::HashSet::new();
    for binding in bindings {
        if &binding.model.provider != provider {
            return Err("provider quota binding uses another provider");
        }
        if binding.limit_ids.is_empty() {
            return Err("provider quota binding must name a pool");
        }
        if binding.limit_ids.len() > MAX_PROVIDER_QUOTA_BINDING_LIMITS {
            return Err("provider quota binding names too many pools");
        }
        if !models.insert(&binding.model) {
            return Err("duplicate provider quota model binding");
        }
        let mut limits = std::collections::HashSet::new();
        if !binding.limit_ids.iter().all(|limit| limits.insert(limit)) {
            return Err("duplicate pool in provider quota binding");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "provider_quota/tests.rs"]
mod tests;
