//! Pending source pages awaiting one canonical notification report.

use std::time::Instant;

use rostra_client::SocialPostMaterializationCursor;

use crate::notification_post::Post;

/// One bounded page awaiting report acknowledgement.
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    /// Cursor after every materialization in the page.
    pub(crate) end: SocialPostMaterializationCursor,
    /// First selected row in monotonic time.
    pub(crate) first_queued_at: Instant,
    /// Last selected row in monotonic time.
    pub(crate) last_queued_at: Instant,
    /// Bounded model-visible prefix.
    pub(crate) preview: Vec<Post>,
    /// Count including omitted previews.
    pub(crate) count: usize,
}
