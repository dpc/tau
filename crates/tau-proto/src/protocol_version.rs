use std::fmt;

use serde::{Deserialize, Serialize};

/// Harness-peer wire and extension-visible event contract revision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Lockstep compatibility generation.
    pub major: u32,
    /// Best-effort compatible revision within a generation.
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
