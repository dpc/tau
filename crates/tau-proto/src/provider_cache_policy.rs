//! Runtime-only provider cache mechanism and renewal metadata.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::{ProviderCachePrivacy, ProviderCacheQuotaAccounting};

#[cfg(test)]
mod tests;

/// Documented cache behavior for one exact provider/model route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCachePolicy {
    /// Provider cache mechanism used by the route.
    pub kind: ProviderCacheKind,
    /// Documented residency timing contract.
    pub ttl: ProviderCacheTtl,
    /// Documented operation that renews residency.
    pub renewal: ProviderCacheRenewal,
    /// Smallest documented output behavior of a renewal operation.
    pub output_floor: ProviderCacheOutputFloor,
    /// Provider quota treatment of a successful renewal attempt.
    pub quota: ProviderCacheQuotaAccounting,
    /// Adapter-owned version of stable-prefix serialization and breakpoint
    /// rules.
    pub prefix_identity_version: NonZeroU32,
    /// Privacy and retention posture of this cache mode.
    pub privacy: ProviderCachePrivacy,
}

/// Provider cache mechanism used by a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheKind {
    /// Provider automatically caches a matching request prefix.
    AutomaticPrefix,
    /// Tau or the provider marks an explicit prefix breakpoint.
    ExplicitBreakpoint,
    /// The provider stores an explicitly named cache object.
    ExplicitObject,
    /// The provider chains requests through response or conversation state.
    ResponseChain,
}

/// Documented provider cache residency timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderCacheTtl {
    /// A qualifying successful read resets an inactivity deadline.
    SlidingKnown {
        /// Documented inactivity lifetime in seconds.
        seconds: NonZeroU64,
    },
    /// Residency is guaranteed for at least this duration, with no hard expiry.
    Minimum {
        /// Documented minimum lifetime in seconds.
        seconds: NonZeroU64,
    },
    /// Creation or expiry patch establishes an absolute lifetime.
    Fixed {
        /// Documented absolute lifetime in seconds.
        seconds: NonZeroU64,
    },
    /// The route exposes no useful documented residency duration.
    Unknown,
}

/// Documented operation that renews provider cache residency.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheRenewal {
    /// A successful qualifying cache read resets a sliding deadline.
    Read,
    /// A typed explicit-object operation patches expiry without generation.
    PatchExpiry,
    /// Renewal requires recreating or rewriting equivalent cache state.
    Recreate,
    /// Tau knows no documented renewal operation.
    Unsupported,
}

/// Smallest documented output behavior of a cache renewal operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheOutputFloor {
    /// The renewal operation is documented as non-generating.
    Zero,
    /// The route requires a positive one-token request floor.
    One,
    /// Hidden reasoning can exceed a nominal output cap.
    UnboundedReasoning,
    /// The route exposes no usable output bound.
    Unknown,
}
