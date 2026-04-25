//! Structured diff payload returned by file-mutation tools.
//!
//! Tools that change file contents (`write`, `edit`) attach a
//! [`DiffSummary`] to their result so UIs can render it however they
//! like — themed colors, expand/collapse, intra-line highlighting,
//! jump-to-changed-line — instead of being handed a pre-formatted
//! string.
//!
//! On the wire this rides as a CBOR sub-tree under the tool result's
//! `diff` field. Encode/decode go through serde so the schema stays
//! single-sourced.

use serde::{Deserialize, Serialize};

/// Top-level structured diff for one file mutation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Number of added lines, summed across hunks. Drives the
    /// `+N/-M` chip in the default UI summary.
    pub added: u32,
    pub removed: u32,
    /// Empty when the file is unchanged.
    pub hunks: Vec<DiffHunk>,
}

/// A contiguous changed region with surrounding context. Maps directly
/// to a unified-diff `@@ -old_start,old_count +new_start,new_count @@`
/// header.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

/// One line of a hunk, tagged for renderer dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffLine {
    Equal {
        text: String,
    },
    Remove {
        text: String,
    },
    Add {
        text: String,
    },
    /// One Remove paired with one Add, with intra-line word-level
    /// segments. Pi's trick: when only a few tokens on a line changed,
    /// rendering full red+green is noisy; `Modify` lets the UI
    /// highlight just the changed slices.
    Modify {
        old: Vec<DiffSegment>,
        new: Vec<DiffSegment>,
    },
}

/// One sub-line slice inside a `DiffLine::Modify`. The renderer paints
/// `Equal` plain, `Remove`/`Add` inverted in red/green so changed
/// tokens pop out of the surrounding context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffSegment {
    Equal { text: String },
    Remove { text: String },
    Add { text: String },
}
