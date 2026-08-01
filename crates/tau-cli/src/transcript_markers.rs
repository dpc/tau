//! Fixed semantic markers for transcript rows other than user and agent turns.
//!
//! User-prompt markers remain configurable, while these markers identify the
//! event category without changing its text or styling.

/// Marker for a semantic message: agent-to-agent or external communication,
/// plus extension-originated and harness-originated prompts without a
/// front-exact queued user projection.
pub(crate) const MESSAGE: &str = "■ ";

/// Marker for a harness-authored structured status update.
pub(crate) const STATUS_UPDATE: &str = "▤ ";

/// Marker for a harness or local UI notice.
pub(crate) const NOTICE: &str = "□ ";
