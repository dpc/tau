//! Owns prompt contents, editing history, and prompt-local viewport state.

use crate::{CompletionMenu, HistoryNav, PromptDraft, PromptSnapshot, StyledText};

/// Prompt contents, editing history, and prompt-local viewport state.
pub(super) struct PromptEditorState {
    /// Prompt rendered to the left of the editable input.
    pub(super) left_prompt: StyledText,
    /// Prompt rendered at the right edge of the terminal.
    pub(super) right_prompt: StyledText,
    /// Placeholder rendered while the editable input is empty.
    pub(super) input_placeholder: StyledText,
    /// Current editable input.
    pub(super) buffer: String,
    /// Monotonic revision advanced before every raw key/paste edit attempt and
    /// every external buffer replacement.
    pub(super) revision: u64,
    /// Exact post-clear revision captured for the most recently submitted line.
    pub(super) last_submitted_revision: Option<u64>,
    /// Byte offset of the input cursor.
    pub(super) cursor: usize,
    /// Visual column the cursor "wants" to be on for vertical motion
    /// (Up/Down within the buffer and across history). Lazily set on
    /// the first vertical motion after a horizontal motion or edit,
    /// then preserved across consecutive vertical motions so jumping
    /// over short or empty lines doesn't permanently truncate the
    /// column. Cleared by any cursor change that isn't a vertical
    /// motion.
    pub(super) sticky_col: Option<usize>,
    /// Bounded newest suffix of submitted lines. Each entry carries its own
    /// undo/redo stacks so history navigation can preserve draft-local
    /// editing state.
    pub(super) input_history: Vec<PromptDraft>,
    /// Undo snapshots for the current editable input.
    pub(super) current_undo: Vec<PromptSnapshot>,
    /// Redo snapshots for the current editable input.
    pub(super) current_redo: Vec<PromptSnapshot>,
    /// Active history navigation, if any. Independent of `completion`.
    pub(super) history_nav: Option<HistoryNav>,
    /// Source history entry edited by the most recently submitted line.
    pub(super) last_submitted_recalled_source: Option<usize>,
    /// Whether the most recently submitted line survived history retention.
    pub(super) last_submitted_input_retained: bool,
    /// Active completion menu, if any. Independent of `history_nav`.
    pub(super) completion: Option<CompletionMenu>,
    /// First visual input row rendered in the prompt-local capped viewport.
    /// This is independent of terminal scrollback/history viewporting; plain
    /// Up/Down can adjust it before falling through to prompt history.
    pub(super) input_viewport_start: usize,
    /// Whether to show a compact indicator when prompt input rows are hidden.
    pub(super) show_prompt_scroll_indicator: bool,
    /// Whether an empty-prompt Ctrl-C has armed cancel for a second press.
    pub(super) ctrl_c_cancel_armed: bool,
}

impl PromptEditorState {
    /// Creates an empty editor with the supplied left prompt.
    pub(super) fn new(left_prompt: StyledText) -> Self {
        Self {
            left_prompt,
            right_prompt: StyledText::new(),
            input_placeholder: StyledText::new(),
            buffer: String::new(),
            revision: 0,
            last_submitted_revision: None,
            cursor: 0,
            sticky_col: None,
            input_history: Vec::new(),
            current_undo: Vec::new(),
            current_redo: Vec::new(),
            history_nav: None,
            last_submitted_recalled_source: None,
            last_submitted_input_retained: false,
            completion: None,
            input_viewport_start: 0,
            show_prompt_scroll_indicator: true,
            ctrl_c_cancel_armed: false,
        }
    }
}
