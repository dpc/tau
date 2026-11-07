use serde::{Deserialize, Serialize};

use crate::{ArtifactKey, CompactionTransactionId};

/// Observational locator for a standalone compaction's searchable transcript.
///
/// This does not pin the archive or grant compaction/recovery authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecallArtifact {
    /// Accepted standalone transaction whose replaced prefix was archived.
    pub transaction_id: CompactionTransactionId,
    /// Shared-store archive identity, subject to ordinary artifact retention.
    pub key: ArtifactKey,
}
