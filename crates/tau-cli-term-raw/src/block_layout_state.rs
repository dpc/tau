//! Owns rendered output blocks and their placement around the prompt.

use std::collections::HashMap;

use crate::terminal_history_generation::TerminalHistoryGeneration;
use crate::{BlockId, StyledBlock};

/// Rendered output blocks and their placement around the prompt.
pub(super) struct BlockLayoutState {
    /// Central block storage.
    pub(super) blocks: HashMap<BlockId, StyledBlock>,
    /// Human-readable labels for diagnostics.
    pub(super) block_debug_ids: HashMap<BlockId, String>,
    /// Next auto-increment id.
    pub(super) next_id: u64,
    /// Persistent output — append-only ordered list of block ids.
    pub(super) history: Vec<BlockId>,
    /// Reference count of block ids present in `history`.
    pub(super) history_refs: HashMap<BlockId, usize>,
    /// Bumped whenever persistent history content, order, or layout changes.
    pub(super) history_generation: TerminalHistoryGeneration,
    /// Earliest history entry changed since the redraw cache last refreshed.
    ///
    /// Ordinary output appends mark only the new suffix. Destructive or
    /// content-changing operations conservatively invalidate from entry zero.
    pub(super) history_dirty_from: Option<usize>,
    /// Mutable blocks above the prompt (can be reordered).
    pub(super) above_active: Vec<BlockId>,
    /// Blocks pinned right above the prompt.
    pub(super) above_sticky: Vec<BlockId>,
    /// Blocks rendered immediately below the input line (e.g. completion
    /// menus), between the prompt and `below`.
    pub(super) suggestions: Vec<BlockId>,
    /// Blocks rendered below suggestions.
    pub(super) below: Vec<BlockId>,
}

impl BlockLayoutState {
    /// Creates empty block storage and placement zones.
    pub(super) fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            block_debug_ids: HashMap::new(),
            next_id: 0,
            history: Vec::new(),
            history_refs: HashMap::new(),
            history_generation: TerminalHistoryGeneration::default(),
            history_dirty_from: None,
            above_active: Vec::new(),
            above_sticky: Vec::new(),
            suggestions: Vec::new(),
            below: Vec::new(),
        }
    }
}
