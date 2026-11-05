use serde::{Deserialize, Serialize};

/// Self-reported package identity for diagnostics, not protocol admission or
/// trust.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildIdentity {
    /// Release version of the reporting package, not the SDK.
    pub release: String,
    /// Source revision, optionally suffixed with `-modified`; absent if
    /// unavailable.
    pub revision: Option<String>,
}
