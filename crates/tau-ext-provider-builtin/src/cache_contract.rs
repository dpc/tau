//! Validated runtime cache contract metadata for generic provider models.

use std::num::NonZeroU32;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

#[cfg(test)]
mod tests;

/// Operator-declared cache semantics for one exact generic provider route.
///
/// Tau publishes this static declaration as runtime model metadata. It never
/// treats the declaration as cache residency, lifecycle, or scheduling state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderCacheContract {
    /// Provider cache mechanism used by the route.
    kind: tau_proto::ProviderCacheKind,
    /// Documented residency timing contract.
    ttl: tau_proto::ProviderCacheTtl,
    /// Documented operation that renews residency.
    renewal: tau_proto::ProviderCacheRenewal,
    /// Smallest documented output behavior of renewal.
    output_floor: tau_proto::ProviderCacheOutputFloor,
    /// Provider quota treatment of a successful renewal attempt.
    quota: tau_proto::ProviderCacheQuotaAccounting,
    /// Privacy and retention posture of this cache mode.
    privacy: tau_proto::ProviderCachePrivacy,
}

impl ProviderCacheContract {
    /// Convert configured facts into the runtime protocol contract.
    pub fn runtime_policy(self) -> Result<tau_proto::ProviderCachePolicy, &'static str> {
        let validated = self.validate()?;
        Ok(tau_proto::ProviderCachePolicy {
            kind: validated.kind,
            ttl: validated.ttl,
            renewal: validated.renewal,
            output_floor: validated.output_floor,
            quota: validated.quota,
            prefix_identity_version: NonZeroU32::new(1).expect("one is nonzero"),
            privacy: validated.privacy,
        })
    }

    /// Reject provider-independent contradictions and unsupported lifecycle
    /// claims.
    fn validate(self) -> Result<Self, &'static str> {
        if self.renewal == tau_proto::ProviderCacheRenewal::Read
            && !matches!(self.ttl, tau_proto::ProviderCacheTtl::SlidingKnown { .. })
        {
            return Err("cache_contract renewal `read` requires ttl `sliding_known`");
        }
        if self.renewal == tau_proto::ProviderCacheRenewal::PatchExpiry
            && !(self.kind == tau_proto::ProviderCacheKind::ExplicitObject
                && matches!(self.ttl, tau_proto::ProviderCacheTtl::Fixed { .. }))
        {
            return Err(
                "cache_contract renewal `patch_expiry` requires explicit_object with fixed ttl",
            );
        }
        if self.privacy.storage == tau_proto::ProviderCacheStorageMode::NamedProviderObject
            && self.kind != tau_proto::ProviderCacheKind::ExplicitObject
        {
            return Err(
                "cache_contract named_provider_object storage requires explicit_object kind",
            );
        }
        if self.privacy.manual_deletion == tau_proto::ProviderCacheDeletionAvailability::Supported {
            return Err(
                "cache_contract manual deletion is unsupported by current production backends",
            );
        }
        Ok(self)
    }
}

/// Raw cache contract decoded before validating cross-field invariants.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnvalidatedProviderCacheContract {
    /// Provider cache mechanism used by the route.
    kind: tau_proto::ProviderCacheKind,
    /// Documented residency timing contract.
    ttl: ConfiguredCacheTtl,
    /// Documented operation that renews residency.
    renewal: tau_proto::ProviderCacheRenewal,
    /// Smallest documented output behavior of renewal.
    output_floor: tau_proto::ProviderCacheOutputFloor,
    /// Provider quota treatment of a successful renewal attempt.
    quota: ConfiguredCacheQuotaAccounting,
    /// Privacy and retention posture of this cache mode.
    privacy: ConfiguredCachePrivacy,
}

/// Strict config-only cache TTL shape.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ConfiguredCacheTtl {
    /// A documented sliding inactivity lifetime.
    SlidingKnown {
        /// Positive lifetime in seconds.
        seconds: std::num::NonZeroU64,
    },
    /// A documented minimum lifetime without a hard expiry.
    Minimum {
        /// Positive minimum lifetime in seconds.
        seconds: std::num::NonZeroU64,
    },
    /// A documented absolute lifetime.
    Fixed {
        /// Positive absolute lifetime in seconds.
        seconds: std::num::NonZeroU64,
    },
    /// No useful documented lifetime.
    Unknown,
}

impl From<ConfiguredCacheTtl> for tau_proto::ProviderCacheTtl {
    fn from(value: ConfiguredCacheTtl) -> Self {
        match value {
            ConfiguredCacheTtl::SlidingKnown { seconds } => Self::SlidingKnown { seconds },
            ConfiguredCacheTtl::Minimum { seconds } => Self::Minimum { seconds },
            ConfiguredCacheTtl::Fixed { seconds } => Self::Fixed { seconds },
            ConfiguredCacheTtl::Unknown => Self::Unknown,
        }
    }
}

/// Strict config-only quota-accounting shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredCacheQuotaAccounting {
    /// Request-count quota treatment.
    requests: tau_proto::ProviderCacheQuotaCharge,
    /// Cache-read token quota treatment.
    read_tokens: tau_proto::ProviderCacheQuotaCharge,
    /// Cache-write token quota treatment.
    write_tokens: tau_proto::ProviderCacheQuotaCharge,
    /// Output-token quota treatment.
    output_tokens: tau_proto::ProviderCacheQuotaCharge,
}

impl From<ConfiguredCacheQuotaAccounting> for tau_proto::ProviderCacheQuotaAccounting {
    fn from(value: ConfiguredCacheQuotaAccounting) -> Self {
        Self {
            requests: value.requests,
            read_tokens: value.read_tokens,
            write_tokens: value.write_tokens,
            output_tokens: value.output_tokens,
        }
    }
}

/// Strict config-only cache privacy shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredCachePrivacy {
    /// Provider storage mechanism.
    storage: tau_proto::ProviderCacheStorageMode,
    /// Zero-data-retention compatibility.
    zero_data_retention: tau_proto::ProviderCacheZeroDataRetentionCompatibility,
    /// Effect on selected-route data residency.
    data_residency: tau_proto::ProviderCacheDataResidencyEffect,
    /// Manual deletion availability.
    manual_deletion: tau_proto::ProviderCacheDeletionAvailability,
}

impl From<ConfiguredCachePrivacy> for tau_proto::ProviderCachePrivacy {
    fn from(value: ConfiguredCachePrivacy) -> Self {
        Self {
            storage: value.storage,
            zero_data_retention: value.zero_data_retention,
            data_residency: value.data_residency,
            manual_deletion: value.manual_deletion,
        }
    }
}

impl<'de> Deserialize<'de> for ProviderCacheContract {
    /// Decode and validate one static generic-route cache declaration.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UnvalidatedProviderCacheContract::deserialize(deserializer)?;
        Self {
            kind: raw.kind,
            ttl: raw.ttl.into(),
            renewal: raw.renewal,
            output_floor: raw.output_floor,
            quota: raw.quota.into(),
            privacy: raw.privacy.into(),
        }
        .validate()
        .map_err(D::Error::custom)
    }
}
