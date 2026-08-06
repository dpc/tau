//! Provider cache privacy and retention metadata.

use serde::{Deserialize, Serialize};

/// Privacy and retention posture of one provider cache mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCachePrivacy {
    /// Provider storage mechanism used by the cache.
    pub storage: ProviderCacheStorageMode,
    /// Compatibility with zero-data-retention requirements.
    pub zero_data_retention: ProviderCacheZeroDataRetentionCompatibility,
    /// Effect on the selected route's data-residency policy.
    pub data_residency: ProviderCacheDataResidencyEffect,
    /// Whether a typed manual deletion operation is available.
    pub manual_deletion: ProviderCacheDeletionAvailability,
}

/// Provider storage mechanism used by a cache mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheStorageMode {
    /// Short-lived provider memory without a named server-side object.
    VolatileMemory,
    /// The mode deliberately extends provider retention.
    ExtendedProviderRetention,
    /// The mode creates a named server-side provider object.
    NamedProviderObject,
    /// Storage semantics belong to the configured compatibility proxy.
    ProxySpecific,
    /// The route exposes no reliable storage classification.
    Unknown,
}

/// Compatibility of a cache mode with zero-data-retention requirements.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheZeroDataRetentionCompatibility {
    /// The provider documents the mode as compatible.
    Compatible,
    /// The provider documents the mode as incompatible.
    Incompatible,
    /// Compatibility requires a provider/surface-specific check.
    ProviderSpecific,
    /// Compatibility is not known.
    Unknown,
}

/// Effect of a cache mode on route-level data-residency policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheDataResidencyEffect {
    /// The cache adds no known residency change beyond the selected route.
    PreservesRoutePolicy,
    /// The exact provider/hosting surface owns the residency effect.
    ProviderSpecific,
    /// The residency effect is not known.
    Unknown,
}

/// Availability of manual provider-cache deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheDeletionAvailability {
    /// An existing typed backend operation can delete the cache.
    Supported,
    /// No manual clear operation is available.
    Unavailable,
    /// Manual deletion availability is not known.
    Unknown,
}
