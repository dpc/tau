//! Provider cache quota-accounting metadata.

use serde::{Deserialize, Serialize};

/// Provider quota treatment of one successful cache renewal attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCacheQuotaAccounting {
    /// Request-count quota treatment.
    pub requests: ProviderCacheQuotaCharge,
    /// Cache-read token quota treatment.
    pub read_tokens: ProviderCacheQuotaCharge,
    /// Cache-write token quota treatment.
    pub write_tokens: ProviderCacheQuotaCharge,
    /// Output-token quota treatment.
    pub output_tokens: ProviderCacheQuotaCharge,
}

/// Treatment of one cache operation class by applicable provider quota pools.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheQuotaCharge {
    /// The class counts fully against its applicable quota pool.
    CountsFully,
    /// The class is documented as exempt from its applicable quota pool.
    Exempt,
    /// A provider-specific evaluator must apply documented model/surface rules.
    ProviderSpecific,
    /// No reliable quota treatment is documented.
    Unknown,
}
