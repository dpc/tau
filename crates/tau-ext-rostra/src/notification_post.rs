//! Projected Rostra post content for one notification report.

use rostra_client::ExternalEventId;

/// One locally projected social post selected by durable reconciliation.
#[derive(Clone, Debug)]
pub(crate) struct Post {
    /// Canonical Rostra external identifier used as a batch-local reference.
    pub(crate) id: ExternalEventId,
    /// Rostra author identity.
    pub(crate) author: String,
    /// Author-provided post timestamp.
    pub(crate) timestamp: String,
    /// Projected persona tags selected by the author.
    pub(crate) persona_tags: String,
    /// Projected Djot source, which remains hostile external content.
    pub(crate) body: String,
}
