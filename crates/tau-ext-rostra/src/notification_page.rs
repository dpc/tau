//! One completed bounded source page ready for notification-state merging.

use rostra_client::SocialPostMaterializationCursor;

use crate::notification_post::Post;

/// A source page's cursor, completion state, and selected projected posts.
pub(crate) struct ScannedPage {
    /// Cursor after every materialization read from the source page.
    pub(crate) scanned_through: SocialPostMaterializationCursor,
    /// Whether the source page contained any rows, including unselected rows.
    pub(crate) had_items: bool,
    /// Whether this page reached the source snapshot's end.
    pub(crate) exhausted: bool,
    /// Bounded projected selected-post prefix from this page.
    pub(crate) preview: Vec<Post>,
    /// Total selected posts, including rows omitted from `preview`.
    pub(crate) count: usize,
}
