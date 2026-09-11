use std::fmt;

use serde::{Deserialize, Serialize};

/// Harness-peer wire and extension-visible event contract revision.
///
/// `SPEC-extension-protocol-versioning` defines the major/minor policy.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Compatibility generation, advanced for changes whose mixed versions
    /// cannot operate even in a workable degraded state.
    pub major: u32,
    /// Best-effort revision. Different values within one generation permit
    /// some workable degraded operation.
    pub minor: u32,
}

impl ProtocolVersion {
    /// Creates an explicit protocol revision.
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}
