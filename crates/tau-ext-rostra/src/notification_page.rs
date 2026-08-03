//! One completed bounded source page ready for notification-state merging.

use rostra_client::SocialPostMaterializationCursor;

/// A source page's cursor, completion state, and selected-post count.
pub(crate) struct ScannedPage {
    /// Cursor after every materialization read from the source page.
    pub(crate) scanned_through: SocialPostMaterializationCursor,
    /// Whether the source page contained any rows, including unselected rows.
    pub(crate) had_items: bool,
    /// Whether this page reached the source snapshot's end.
    pub(crate) exhausted: bool,
    /// Total selected posts in this page.
    pub(crate) count: usize,
}
