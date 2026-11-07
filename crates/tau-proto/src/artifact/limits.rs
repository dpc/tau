use serde::{Deserialize, Serialize};

use super::ArtifactSize;

/// Startup operational artifact policy, bounded by the protocol object ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactLimits {
    /// Maximum complete stored original, including any directory-format header.
    pub stored_bytes: ArtifactSize,
    /// Maximum aggregate regular-file content extracted from a directory.
    pub expanded_bytes: ArtifactSize,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        let size = ArtifactSize::new(32 * 1024 * 1024).expect("default fits protocol ceiling");
        Self {
            stored_bytes: size,
            expanded_bytes: size,
        }
    }
}
