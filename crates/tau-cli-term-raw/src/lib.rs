//! Terminal prompt with async output support.
//!
//! Renders directly to the normal terminal buffer (no alternate screen)
//! so the terminal's native scrollback is preserved. See `README.md`
//! in this crate for the full rendering strategy.
//!
//! Three rendering paths (see `README.md`):
//! - **Differential update** — common case, diffs visible viewport via
//!   [`Screen`]
//! - **Scrolling render** — on overflow, diffs a suffix rebased at the prior
//!   viewport and renders in order; `\r\n` at the bottom pushes content into
//!   scrollback without materializing older hidden history
//! - **Full render** — on resize/invalidation, clears screen + scrollback and
//!   replays the capped log/history suffix plus fixed tail without rubber

mod block_layout_state;
mod presentation_mutation_generation;
mod presentation_observation_state;
mod prompt_editor_state;
mod redraw_sync_generation;
mod renderer_delivery_id;
#[cfg(test)]
mod terminal_generation_tests;
mod terminal_history_generation;
mod terminal_runtime_state;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufWriter, Write};
use std::sync::{Arc, Mutex, MutexGuard, atomic as path_std_sync_atomic};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{sync as path_std_sync, time as path_std_time};

use base64::engine as path_base64_engine;
use crossterm::cursor as path_crossterm_cursor;
const PROMPT_INPUT_MAX_HEIGHT_PERCENT: usize = 33;
/// Maximum number of nonempty drafts retained for one terminal attachment.
const INPUT_HISTORY_MAX_ENTRIES: usize = 1000;
/// Maximum primary UTF-8 text retained for one terminal attachment's drafts.
const INPUT_HISTORY_MAX_BYTES: usize = 16 * 1024 * 1024;
const STALL_WARNING_INTERVAL: Duration = Duration::from_secs(5);
static STALL_WARNING_LIMITER: Mutex<StallWarningLimiter> =
    Mutex::new(StallWarningLimiter { last: None });

/// Independent raw-input history retention limits for one attachment.
#[derive(Clone, Copy)]
struct InputHistoryLimits {
    /// Maximum retained nonempty drafts.
    max_entries: usize,
    /// Maximum retained primary UTF-8 bytes.
    max_bytes: usize,
}

/// Limits repeated slow-stage warnings across terminal operations.
struct StallWarningLimiter {
    /// Most recent admitted warning timestamp.
    last: Option<std::time::Instant>,
}

impl StallWarningLimiter {
    /// Admits the first warning in each fixed minimum interval.
    fn admit(&mut self, now: std::time::Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < STALL_WARNING_INTERVAL)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

fn admit_stall_warning() -> bool {
    STALL_WARNING_LIMITER
        .lock()
        .expect("stall warning mutex poisoned")
        .admit(path_std_time::Instant::now())
}

use block_layout_state::BlockLayoutState;
use crossterm::cursor::{MoveToColumn, MoveUp, SetCursorStyle};
use crossterm::event::{
    self, DisableMouseCapture, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::Print;
use crossterm::{QueueableCommand, terminal};
use presentation_observation_state::{
    CapturedPresentationObservations, PresentationObservationState,
};
pub use presentation_observation_state::{
    OpaquePresentationFact, PresentationInvalidation, PresentationObservationKey,
};
use prompt_editor_state::PromptEditorState;
use redraw_sync_generation::RedrawSyncGeneration;
pub use renderer_delivery_id::RendererDeliveryId;
pub use tau_term_screen::{
    Align, BlockId, Cell, Color, PriorityLine, PriorityLineAlignment, PriorityLinePriority,
    PriorityLineTruncation, Span, Style, StyledBlock, StyledText, TwoLineElision,
    sanitize_hyperlink_target,
};
use tau_term_screen::{
    Screen, display_width, emit_styled_cells, layout_block, layout_lines, next_grapheme_boundary,
    previous_grapheme_boundary, truncate_to_width,
};
use terminal_history_generation::TerminalHistoryGeneration;
use terminal_runtime_state::TerminalRuntimeState;
use unicode_segmentation::UnicodeSegmentation;

type NamedActionHandler = fn(&Term) -> Option<Event>;

// Shared source of truth for raw named actions. Keep this table in sync with
// built-in keybindings and any user-facing binding documentation when adding or
// removing actions.
const NAMED_ACTIONS: &[(&str, NamedActionHandler)] = &[
    ("accept-completion", Term::accept_completion_event),
    ("backtab", Term::backtab_action),
    ("clear-prompt", Term::clear_prompt_action),
    (
        "clear-or-cancel-prompt",
        Term::clear_or_cancel_prompt_action,
    ),
    ("cursor-down", Term::cycle_or_move_down),
    ("cursor-end", Term::move_cursor_end_action),
    ("cursor-left", Term::move_cursor_left_action),
    ("cursor-right", Term::move_cursor_right_action),
    ("cursor-start", Term::move_cursor_start_action),
    ("cursor-up", Term::cycle_or_move_up),
    ("delete-backward", Term::delete_backward_action),
    ("delete-forward", Term::delete_forward_action),
    ("dismiss-completion", Term::dismiss_completion_event),
    ("escape", Term::escape_action),
    ("kill-to-start", Term::kill_to_start_action),
    ("kill-word-left", Term::kill_word_left_action),
    ("move-down", Term::move_cursor_down_action),
    ("move-up", Term::move_cursor_up_action),
    ("prompt-eof", Term::prompt_eof_action),
    (
        "select-completion-next",
        Term::select_completion_next_action,
    ),
    (
        "select-completion-previous",
        Term::select_completion_previous_action,
    ),
];

fn named_action_handler(action: &str) -> Option<NamedActionHandler> {
    NAMED_ACTIONS
        .iter()
        .find_map(|(name, handler)| (*name == action).then_some(*handler))
}

/// Cursor shape requested for the prompt while Tau owns raw mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShape {
    /// Thin vertical cursor bar.
    Bar,
    /// Solid block cursor.
    Block,
}

impl CursorShape {
    fn crossterm_style(self) -> crossterm::cursor::SetCursorStyle {
        match self {
            Self::Bar => path_crossterm_cursor::SetCursorStyle::SteadyBar,
            Self::Block => path_crossterm_cursor::SetCursorStyle::SteadyBlock,
        }
    }
}

/// Immutable terminal behavior selected before Tau acquires the terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalOptions {
    /// Cursor shape Tau uses while it owns raw terminal input.
    pub cursor_shape: CursorShape,
    /// Whether mouse activity remains enabled for the CLI UI.
    ///
    /// When false, the raw terminal layer explicitly disables terminal mouse
    /// reporting while Tau owns the terminal. The terminal then handles mouse
    /// activity natively instead of sending it to Tau.
    pub mouse: bool,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            cursor_shape: CursorShape::Bar,
            mouse: true,
        }
    }
}

/// A single completion candidate surfaced by a [`CompletionSource`].
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Short text shown in the menu's left column.
    pub label: String,
    /// Description shown to the right of the label.
    pub description: String,
    /// Buffer contents to install when this candidate is selected
    /// (preview) or accepted.
    pub replacement: String,
    /// UTF-8 byte offset at which to place the prompt cursor in `replacement`.
    pub cursor: usize,
}

/// Builds the candidate list for the current buffer.
///
/// Called on every buffer mutation (typing, paste, backspace). An
/// empty result closes the completion menu; a non-empty result opens
/// it (or refreshes it if already open).
pub trait CompletionSource: Send + Sync {
    /// Returns whole-buffer completion candidates for `buffer`.
    ///
    /// `cursor` is a UTF-8 byte offset into `buffer`, clamped to a grapheme
    /// boundary by the prompt before this hook is called. The hook runs
    /// synchronously on the input-event path, so implementations should avoid
    /// blocking work. Accepting or previewing a returned candidate replaces the
    /// entire prompt buffer with [`Candidate::replacement`] and places the
    /// cursor at [`Candidate::cursor`]. Sources must provide a UTF-8 byte
    /// offset on an extended-grapheme boundary no larger than the
    /// replacement length; malformed candidates are omitted.
    fn candidates(&self, buffer: &str, cursor: usize) -> Vec<Candidate>;
}

impl<F> CompletionSource for F
where
    F: Fn(&str, usize) -> Vec<Candidate> + Send + Sync,
{
    fn candidates(&self, buffer: &str, cursor: usize) -> Vec<Candidate> {
        (self)(buffer, cursor)
    }
}

/// Read-only snapshot of the completion menu state.
#[derive(Clone, Debug)]
pub struct CompletionView {
    /// Candidates currently displayed in menu order.
    pub candidates: Vec<Candidate>,
    /// Candidate currently previewed in the input buffer, if any.
    pub selected: Option<usize>,
}

#[derive(Clone)]
struct PromptSnapshot {
    buffer: String,
    cursor: usize,
}

#[derive(Clone)]
struct PromptDraft {
    buffer: String,
    cursor: usize,
    undo: Vec<PromptSnapshot>,
    redo: Vec<PromptSnapshot>,
}

impl PromptDraft {
    fn submitted(buffer: String) -> Self {
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

/// One draft navigable through prompt history with its retained-source link.
struct HistoryNavEntry {
    /// Draft content and local undo/redo state shown at this navigation slot.
    draft: PromptDraft,
    /// Original retained history position, absent for queued and new WIP
    /// drafts.
    source_index: Option<usize>,
}

/// State for input-history navigation. Present only while Up/Down
/// has recalled a previous line and the user hasn't submitted or
/// dismissed yet.
struct HistoryNav {
    /// Snapshot of retained entries plus queued/WIP drafts. Editing in history
    /// mode mutates the selected entry's draft and retained source.
    entries: Vec<HistoryNavEntry>,
    /// Current position within `entries`.
    index: usize,
}

/// State for an open completion menu.
struct CompletionMenu {
    candidates: Vec<Candidate>,
    /// `None` = menu open but no preview (buffer == `original_buffer`);
    /// `Some(i)` = candidate `i` is previewed in the buffer.
    selected: Option<usize>,
    original_buffer: String,
    original_cursor: usize,
}

/// Mutable state shared between the input loop, redraw thread, and
/// any [`TermHandle`] holders.
struct SharedState {
    /// Rendered output blocks and their placement around the prompt.
    layout: BlockLayoutState,
    /// Prompt contents, editing history, and prompt-local viewport state.
    editor: PromptEditorState,
    /// Terminal dimensions, redraw coordination, and lifecycle flags.
    terminal: TerminalRuntimeState,
    /// Bounded selected-mutation correlation captured with redraw preparation.
    presentation_observations: PresentationObservationState,
    /// Actual redraw-loop failure timing retained for focused test assertions.
    #[cfg(test)]
    presentation_failure_test_records: Vec<(&'static str, u128, usize, u64)>,
    /// Optional small retention limits for focused cross-crate tests.
    #[cfg(any(test, feature = "history-retention-test-support"))]
    input_history_limit_override: Option<InputHistoryLimits>,
}

impl SharedState {
    fn new(width: usize, height: usize, left_prompt: StyledText) -> Self {
        Self {
            layout: BlockLayoutState::new(),
            editor: PromptEditorState::new(left_prompt),
            terminal: TerminalRuntimeState::new(width, height),
            presentation_observations: PresentationObservationState::new(),
            #[cfg(test)]
            presentation_failure_test_records: Vec::new(),
            #[cfg(any(test, feature = "history-retention-test-support"))]
            input_history_limit_override: None,
        }
    }

    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.layout.next_id);
        self.layout.next_id += 1;
        id
    }

    fn mark_history_dirty_from(&mut self, entry: usize) {
        self.layout.history_generation.advance();
        self.layout.history_dirty_from = Some(
            self.layout
                .history_dirty_from
                .map_or(entry, |dirty| dirty.min(entry)),
        );
    }

    fn add_history_ref(&mut self, id: BlockId) {
        *self.layout.history_refs.entry(id).or_insert(0) += 1;
    }

    fn append_history(&mut self, id: BlockId) {
        let appended_at = self.layout.history.len();
        self.layout.history.push(id);
        self.add_history_ref(id);
        self.mark_history_dirty_from(appended_at);
    }

    fn remove_history_refs(&mut self, id: BlockId, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(existing) = self.layout.history_refs.get_mut(&id) {
            if *existing <= count {
                self.layout.history_refs.remove(&id);
            } else {
                *existing -= count;
            }
        }
    }

    fn rebuild_history_refs(&mut self) {
        self.layout.history_refs.clear();
        for &id in &self.layout.history {
            *self.layout.history_refs.entry(id).or_insert(0) += 1;
        }
        self.mark_history_dirty_from(0);
    }

    fn block_in_history(&self, id: BlockId) -> bool {
        self.layout.history_refs.contains_key(&id)
    }

    /// Returns whether a block contributes to any rendered output zone.
    fn block_is_visible(&self, id: BlockId) -> bool {
        self.block_in_history(id)
            || self.layout.above_active.contains(&id)
            || self.layout.above_sticky.contains(&id)
            || self.layout.suggestions.contains(&id)
            || self.layout.below.contains(&id)
    }

    /// Removes one block and every one of its rendered-zone references.
    fn remove_block(&mut self, id: BlockId, observe_delta: bool) -> bool {
        let presentation_changed = observe_delta && self.block_is_visible(id);
        let existed = self.layout.blocks.remove(&id).is_some();
        let debug_id = self.layout.block_debug_ids.remove(&id);

        // `history_refs` is the authoritative membership index. Queued/live
        // blocks occupy an active zone but not persistent history, so skipping
        // this scan is both safe and keeps their removal independent of
        // transcript length.
        if self.block_in_history(id) {
            #[cfg(test)]
            {
                self.layout.history_removal_scan_entries += self.layout.history.len();
            }
            let removal = remove_all_from_zone(&mut self.layout.history, id);
            let indexed_refs = self
                .layout
                .history_refs
                .get(&id)
                .copied()
                .expect("history membership index must contain referenced block");
            debug_assert_eq!(
                removal.count, indexed_refs,
                "history membership index must exactly count duplicate references"
            );
            self.remove_history_refs(id, removal.count);
            self.mark_history_dirty_from(
                removal
                    .first_index
                    .expect("history membership index must imply one matching entry"),
            );
        }

        remove_all_from_zone(&mut self.layout.above_active, id);
        remove_all_from_zone(&mut self.layout.above_sticky, id);
        remove_all_from_zone(&mut self.layout.suggestions, id);
        remove_all_from_zone(&mut self.layout.below, id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, ?debug_id, existed, "remove block");
        presentation_changed
    }

    fn current_snapshot(&self) -> PromptSnapshot {
        PromptSnapshot {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
        }
    }

    fn current_draft(&self) -> PromptDraft {
        PromptDraft {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
            undo: self.editor.current_undo.clone(),
            redo: self.editor.current_redo.clone(),
        }
    }

    /// Evicts the oldest draft prefix until the retained history is a bounded
    /// newest suffix of nonempty primary prompt text.
    fn limit_input_history(&mut self) {
        let limits = self.input_history_limits();
        let recalled_source = self.editor.last_submitted_recalled_source;
        let mut retained_source = None;
        let mut original_entries = 0;
        let mut retained_entries = 0;
        self.editor.input_history.retain(|draft| {
            let retain = !draft.buffer.is_empty() && draft.buffer.len() <= limits.max_bytes;
            if retain && recalled_source == Some(original_entries) {
                retained_source = Some(retained_entries);
            }
            original_entries += 1;
            retained_entries += usize::from(retain);
            retain
        });
        if self.editor.input_history.len() != original_entries {
            self.editor.history_nav = None;
        }
        self.editor.last_submitted_recalled_source = retained_source;

        let mut retained_bytes = 0;
        let mut retained_start = self.editor.input_history.len();
        for (retained_entries, (index, draft)) in self
            .editor
            .input_history
            .iter()
            .enumerate()
            .rev()
            .enumerate()
        {
            if retained_entries == limits.max_entries
                || draft.buffer.len() > limits.max_bytes - retained_bytes
            {
                break;
            }
            retained_bytes += draft.buffer.len();
            retained_start = index;
        }
        if retained_start == 0 {
            return;
        }

        self.editor.input_history.drain(..retained_start);
        self.editor.history_nav = None;
        self.editor.last_submitted_recalled_source = self
            .editor
            .last_submitted_recalled_source
            .and_then(|index| index.checked_sub(retained_start));
    }

    /// Returns production limits or a focused test's intentionally small pair.
    fn input_history_limits(&self) -> InputHistoryLimits {
        #[cfg(any(test, feature = "history-retention-test-support"))]
        if let Some(limits) = self.input_history_limit_override {
            return limits;
        }
        InputHistoryLimits {
            max_entries: INPUT_HISTORY_MAX_ENTRIES,
            max_bytes: INPUT_HISTORY_MAX_BYTES,
        }
    }

    /// Takes the current undo state into a submitted history entry.
    ///
    /// Submission clears the live prompt, so moving these stacks preserves raw
    /// history semantics without cloning snapshots that a higher layer may
    /// immediately replace with a canonical submitted draft.
    fn take_submitted_draft(&mut self) -> PromptDraft {
        PromptDraft {
            buffer: self.editor.buffer.clone(),
            cursor: self.editor.cursor,
            undo: std::mem::take(&mut self.editor.current_undo),
            redo: std::mem::take(&mut self.editor.current_redo),
        }
    }

    fn load_draft(&mut self, draft: PromptDraft) {
        self.editor.buffer = draft.buffer;
        self.editor.current_undo = draft.undo;
        self.editor.current_redo = draft.redo;
        self.editor.cursor = draft.cursor.min(self.editor.buffer.len());
        self.ensure_input_cursor_visible();
    }

    fn record_undo(&mut self) {
        self.editor.current_undo.push(self.current_snapshot());
        self.editor.current_redo.clear();
    }

    /// Mirrors edits made to `buffer` and undo state into the live
    /// history-nav slot so navigating Down then Up returns to the
    /// user's edited copy. No-op when not navigating history.
    fn sync_buffer_to_history_nav(&mut self) {
        let draft = self.current_draft();
        if let Some(nav) = self.editor.history_nav.as_mut() {
            nav.entries[nav.index].draft = draft.clone();
            if let Some(source_index) = nav.entries[nav.index].source_index
                && let Some(source) = self.editor.input_history.get_mut(source_index)
            {
                *source = draft;
            }
        }
    }

    /// Visual `(row, col)` of the cursor against the current buffer.
    /// Row 0 starts after the left prompt, so `col` on row 0 is offset
    /// by the prompt width.
    fn visual_cursor_position(&self) -> (usize, usize) {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        buffer_position_for_byte(&self.editor.buffer, self.editor.cursor, width, left_cols)
    }

    /// Last visual row index of the current buffer.
    fn last_visual_row(&self) -> usize {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        let (max_row, _) = buffer_end_position(&self.editor.buffer, width, left_cols);
        max_row
    }

    /// Byte offset within the current buffer that lands the cursor at
    /// the given visual `(row, col)`. Clamps to the nearest reachable
    /// position.
    fn cursor_byte_at(&self, target_row: usize, target_col: usize) -> usize {
        let width = self.terminal.width.max(1);
        let left_cols = self.editor.left_prompt.char_count();
        byte_offset_for_buffer_position(
            &self.editor.buffer,
            target_row,
            target_col,
            width,
            left_cols,
        )
    }

    /// Visual column to use for the next vertical motion: returns the
    /// sticky column if one is set, otherwise captures the current
    /// cursor's visual column and stores it as sticky.
    fn vertical_target_col(&mut self) -> usize {
        if let Some(col) = self.editor.sticky_col {
            return col;
        }
        let (_, col) = self.visual_cursor_position();
        self.editor.sticky_col = Some(col);
        col
    }

    /// Sets the cursor as part of a horizontal motion or edit and
    /// invalidates the sticky vertical column. All cursor mutations
    /// outside of vertical motion must go through this — the sticky
    /// column only stays valid as long as the cursor is moving
    /// purely up/down.
    fn write_cursor(&mut self, new_cursor: usize) {
        self.editor.cursor = new_cursor;
        self.editor.sticky_col = None;
        self.ensure_input_cursor_visible();
    }

    /// Sets the cursor as part of a vertical motion. Preserves the
    /// sticky column so consecutive vertical moves can replay the
    /// original column over short or empty rows.
    fn write_cursor_keep_sticky(&mut self, new_cursor: usize) {
        self.editor.cursor = new_cursor;
        self.ensure_input_cursor_visible();
    }

    fn input_visible_rows(&self) -> usize {
        let total_rows = self.last_visual_row() + 1;
        let cap_rows = prompt_input_max_rows(self.terminal.height);
        let indicator_rows = prompt_scroll_indicator_rows(
            self.editor.show_prompt_scroll_indicator,
            !self.editor.buffer.is_empty(),
            total_rows,
            cap_rows,
        );
        prompt_editable_rows(total_rows, cap_rows, indicator_rows)
    }

    fn ensure_input_cursor_visible(&mut self) {
        let (cursor_row, _) = self.visual_cursor_position();
        let total_rows = self.last_visual_row() + 1;
        let visible_rows = self.input_visible_rows();
        self.editor.input_viewport_start = viewport_start_with_cursor(
            self.editor.input_viewport_start,
            cursor_row,
            total_rows,
            visible_rows,
        );
    }

    /// Pushes the current prompt onto input history and resets to a
    /// fresh empty prompt. An empty prompt does not add an entry, but may
    /// still enforce retention while leaving history navigation. Clears the
    /// sticky column via `write_cursor`.
    fn push_current_as_history_entry(&mut self, enforce_limit: bool) -> bool {
        if self.editor.buffer.is_empty() {
            if enforce_limit {
                self.limit_input_history();
            }
            return false;
        }
        let draft = self.take_submitted_draft();
        self.editor.input_history.push(draft);
        if enforce_limit {
            self.limit_input_history();
        }
        self.editor.buffer.clear();
        self.write_cursor(0);
        true
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.editor.current_undo.pop() else {
            return false;
        };
        self.editor.current_redo.push(self.current_snapshot());
        self.editor.buffer = snapshot.buffer;
        self.write_cursor(snapshot.cursor.min(self.editor.buffer.len()));
        self.sync_buffer_to_history_nav();
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = self.editor.current_redo.pop() else {
            return false;
        };
        self.editor.current_undo.push(self.current_snapshot());
        self.editor.buffer = snapshot.buffer;
        self.write_cursor(snapshot.cursor.min(self.editor.buffer.len()));
        self.sync_buffer_to_history_nav();
        true
    }

    /// Cycles the completion menu selection by `delta` (+1 forward,
    /// -1 backward) and updates the buffer to preview the new
    /// selection (or restore `original_buffer` when wrapping past the
    /// ends to `selected = None`). Returns `true` if a menu was open.
    fn cycle_completion(&mut self, delta: isize) -> bool {
        let (new_buffer, new_cursor) = {
            let Some(menu) = self.editor.completion.as_mut() else {
                return false;
            };
            let len = menu.candidates.len();
            if len == 0 {
                return false;
            }
            let new_selected = match menu.selected {
                None => Some(if 0 < delta { 0 } else { len - 1 }),
                // Up at the first match drops back to "no preview" so
                // the user sees their original buffer; pressing Up
                // again wraps to the last match.
                Some(0) if delta < 0 => None,
                Some(i) => Some((i as isize + delta).rem_euclid(len as isize) as usize),
            };
            menu.selected = new_selected;
            match new_selected {
                None => (menu.original_buffer.clone(), menu.original_cursor),
                Some(idx) => {
                    let candidate = &menu.candidates[idx];
                    let buf = candidate.replacement.clone();
                    let cursor = candidate.cursor;
                    (buf, cursor)
                }
            }
        };
        self.editor.buffer = new_buffer;
        self.write_cursor(new_cursor);
        true
    }

    /// Closes the completion menu. If a candidate was previewed,
    /// restores the original buffer; otherwise leaves the buffer
    /// alone. Returns `true` if a menu was open.
    fn dismiss_completion(&mut self) -> bool {
        let Some(menu) = self.editor.completion.take() else {
            return false;
        };
        if menu.selected.is_some() {
            self.editor.buffer = menu.original_buffer;
            self.write_cursor(menu.original_cursor);
        }
        true
    }

    /// Accepts the currently previewed candidate: closes the menu,
    /// leaves the previewed buffer in place. Returns `true` if a
    /// candidate was accepted (i.e. the menu had a selection).
    fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.editor.completion.as_ref() else {
            return false;
        };
        if menu.selected.is_none() {
            return false;
        }
        // Buffer already matches the previewed replacement; just
        // close the menu.
        self.editor.completion = None;
        true
    }

    /// Steps history navigation by `delta`. Enters history-nav mode
    /// from `Editing` when moving backward and history exists. Moving
    /// forward from a non-empty editing buffer stores it as history
    /// and opens a fresh empty prompt. Returns `true` if the buffer
    /// changed.
    ///
    /// Cursor placement preserves the visual column so that
    /// `Up`/`Down` across prompts feels like one continuous text:
    /// stepping back lands on the previous entry's last visual row,
    /// stepping forward lands on the next entry's first visual row,
    /// both at (or clamped to) the column the cursor was on.
    fn step_history(&mut self, delta: isize) -> bool {
        let target_col = self.vertical_target_col();
        if self.editor.history_nav.is_none() {
            if 0 < delta {
                return self.push_current_as_history_entry(true);
            }
            return self.enter_history_nav(target_col);
        }
        self.advance_history_nav(delta, target_col)
    }

    /// Switches from `Editing` into history-navigation mode at the
    /// most recent entry, with the cursor placed at the previous
    /// entry's last visual row at `target_col`.
    fn enter_history_nav(&mut self, target_col: usize) -> bool {
        if self.editor.input_history.is_empty() {
            return false;
        }
        let mut entries: Vec<_> = self
            .editor
            .input_history
            .iter()
            .cloned()
            .enumerate()
            .map(|(source_index, draft)| HistoryNavEntry {
                draft,
                source_index: Some(source_index),
            })
            .collect();
        entries.push(HistoryNavEntry {
            draft: self.current_draft(),
            source_index: None,
        });
        // The WIP buffer sits at `entries.last()`; the previous
        // history entry is one slot before it.
        let index = entries.len() - 2;
        self.load_draft(entries[index].draft.clone());
        let new_cursor = self.cursor_byte_at(self.last_visual_row(), target_col);
        self.write_cursor_keep_sticky(new_cursor);
        self.editor.history_nav = Some(HistoryNav { entries, index });
        true
    }

    fn recall_prompt_before_current(&mut self, text: String) {
        let previous = self.current_draft();
        let previous_source = self.editor.history_nav.as_ref().and_then(|nav| {
            nav.entries
                .get(nav.index)
                .and_then(|entry| entry.source_index)
        });
        let mut entries: Vec<_> = self
            .editor
            .input_history
            .iter()
            .cloned()
            .enumerate()
            .map(|(source_index, draft)| HistoryNavEntry {
                draft,
                source_index: Some(source_index),
            })
            .collect();
        entries.push(HistoryNavEntry {
            draft: PromptDraft::submitted(text),
            source_index: None,
        });
        entries.push(HistoryNavEntry {
            draft: previous,
            source_index: previous_source,
        });
        let index = entries.len() - 2;
        self.load_draft(entries[index].draft.clone());
        self.write_cursor(self.editor.buffer.len());
        self.editor.history_nav = Some(HistoryNav { entries, index });
        self.editor.completion = None;
    }

    /// Steps within an already-active history navigation. Going past
    /// the WIP slot (Down at the latest entry) pushes the WIP buffer
    /// onto history and returns to a fresh prompt, mirroring Down
    /// from `Editing`.
    fn advance_history_nav(&mut self, delta: isize, target_col: usize) -> bool {
        let current = self.current_draft();
        let nav = self
            .editor
            .history_nav
            .as_mut()
            .expect("caller checked Some");
        let new_index = nav.index as isize + delta;
        if new_index < 0 {
            return false;
        }
        if new_index >= nav.entries.len() as isize {
            let wip = nav.entries.last().map(|entry| entry.draft.clone());
            self.editor.history_nav = None;
            if let Some(wip) = wip {
                self.load_draft(wip);
            }
            return self.push_current_as_history_entry(true);
        }
        nav.entries[nav.index].draft = current.clone();
        if let Some(source_index) = nav.entries[nav.index].source_index
            && let Some(source) = self.editor.input_history.get_mut(source_index)
        {
            *source = current;
        }
        nav.index = new_index as usize;
        let new_draft = nav.entries[nav.index].draft.clone();
        self.load_draft(new_draft);
        let target_row = if delta < 0 { self.last_visual_row() } else { 0 };
        let new_cursor = self.cursor_byte_at(target_row, target_col);
        self.write_cursor_keep_sticky(new_cursor);
        true
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum KeyBinding {
    Ctrl(char),
    CtrlShift(char),
    Meta(char),
    CtrlKey(KeyCode),
    Key(KeyCode),
}

fn parse_plain_key_code(input: &str) -> Option<KeyCode> {
    match input.to_ascii_lowercase().as_str() {
        "backspace" => Some(KeyCode::Backspace),
        "backtab" | "shift-tab" => Some(KeyCode::BackTab),
        "delete" | "del" => Some(KeyCode::Delete),
        "down" => Some(KeyCode::Down),
        "end" => Some(KeyCode::End),
        "enter" => Some(KeyCode::Enter),
        "esc" | "escape" => Some(KeyCode::Esc),
        "home" => Some(KeyCode::Home),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "tab" => Some(KeyCode::Tab),
        "up" => Some(KeyCode::Up),
        _ => None,
    }
}

fn parse_key_binding(input: &str) -> Option<KeyBinding> {
    let input = input.trim_matches('`');
    if let Some(code) = parse_plain_key_code(input) {
        return Some(KeyBinding::Key(code));
    }
    if let Some(rest) = input.strip_prefix("M-") {
        let mut chars = rest.chars();
        let ch = chars.next()?;
        return (chars.next().is_none() && ch.is_ascii()).then_some(KeyBinding::Meta(ch));
    }
    let rest = input
        .strip_prefix("C-")
        .or_else(|| input.strip_prefix("c-"))?;
    match rest.to_ascii_lowercase().as_str() {
        "enter" => return Some(KeyBinding::CtrlKey(KeyCode::Enter)),
        "up" => return Some(KeyBinding::CtrlKey(KeyCode::Up)),
        "down" => return Some(KeyBinding::CtrlKey(KeyCode::Down)),
        _ => {}
    }
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if ch.is_ascii_uppercase() {
        Some(KeyBinding::CtrlShift(ch.to_ascii_lowercase()))
    } else {
        Some(KeyBinding::Ctrl(ch.to_ascii_lowercase()))
    }
}

fn key_binding_for_event(key: KeyEvent, ctrl: bool) -> Option<KeyBinding> {
    let modifiers = key.modifiers;
    let plain = modifiers.is_empty();
    let ctrl_only = modifiers == KeyModifiers::CONTROL;

    match key.code {
        KeyCode::Char(ch) if modifiers == KeyModifiers::ALT => Some(KeyBinding::Meta(ch)),
        KeyCode::Char(ch)
            if ctrl
                && ch.is_ascii_alphabetic()
                && (modifiers.contains(KeyModifiers::SHIFT) || ch.is_ascii_uppercase()) =>
        {
            Some(KeyBinding::CtrlShift(ch.to_ascii_lowercase()))
        }
        KeyCode::Char(ch) if ctrl => Some(KeyBinding::Ctrl(ch.to_ascii_lowercase())),
        KeyCode::Char(ch @ '\u{1}'..='\u{1a}') => {
            let letter = (b'a' + ch as u8 - 1) as char;
            Some(KeyBinding::Ctrl(letter))
        }
        KeyCode::Enter if ctrl_only => Some(KeyBinding::CtrlKey(KeyCode::Enter)),
        KeyCode::Up | KeyCode::Down if ctrl_only => Some(KeyBinding::CtrlKey(key.code)),
        KeyCode::BackTab => Some(KeyBinding::Key(KeyCode::BackTab)),
        KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Down
        | KeyCode::End
        | KeyCode::Enter
        | KeyCode::Esc
        | KeyCode::Home
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Tab
        | KeyCode::Up
            if plain =>
        {
            Some(KeyBinding::Key(key.code))
        }
        _ => None,
    }
}
/// High-level events surfaced to the downstream event loop.
pub enum Event {
    /// The user submitted a line with Ctrl-Enter or `submit-prompt`
    /// outside the completion menu, or with no candidate selected.
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The user requested prompt cancellation with a second consecutive Ctrl-C.
    CancelPrompt,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The terminal reported focus gained or lost.
    FocusChanged { focused: bool },
    /// The input buffer or completion menu state changed. Fires for
    /// keystrokes that mutate the buffer and for completion menu
    /// open/close/cycle. Caller should re-render anything that
    /// depends on either (typically the menu and the prompt itself).
    BufferChanged,
    /// The user pressed Ctrl-Enter with a candidate previewed in the
    /// menu. The buffer is now the candidate's replacement and
    /// completion has been re-evaluated for that buffer. The caller
    /// should re-render the menu area but typically *should not*
    /// submit — a second Ctrl-Enter is expected to confirm.
    CompletionAccept,
    /// The user pressed Shift-Tab outside an open completion menu.
    /// Inside a menu it cycles backwards and is consumed internally.
    BackTab,
    /// The user pressed Escape outside an open completion menu.
    Escape,
    /// The user activated a configured key binding.
    Binding(String),
    /// A local prompt notice should be printed above the prompt.
    Notice(String),
    /// The user requested an external editor (Ctrl-O / Ctrl-G).
    /// Caller is expected to call [`Term::pause_for_external`], spawn
    /// `$VISUAL`/`$EDITOR`, and call [`Term::resume_after_external`].
    ExternalEditor,
}

/// References removed from one ordered render zone.
#[derive(Default)]
struct ZoneRemoval {
    /// Number of removed references.
    count: usize,
    /// First entry whose removal changes the remaining suffix.
    first_index: Option<usize>,
}

/// Removes every occurrence of `id` from one rendered zone.
fn remove_all_from_zone(zone: &mut Vec<BlockId>, id: BlockId) -> ZoneRemoval {
    let mut removal = ZoneRemoval::default();
    let mut index = 0;
    zone.retain(|&candidate| {
        let current_index = index;
        index += 1;
        if candidate == id {
            removal.count += 1;
            removal.first_index.get_or_insert(current_index);
            false
        } else {
            true
        }
    });
    removal
}

/// Snapshot of terminal output zones, excluding prompt input/history state.
#[derive(Clone, Debug, Default)]
pub struct OutputSnapshot {
    blocks: HashMap<BlockId, StyledBlock>,
    block_debug_ids: HashMap<BlockId, String>,
    /// Next block identity allocated within this presentation model.
    next_id: u64,
    history: Vec<BlockId>,
    above_active: Vec<BlockId>,
    above_sticky: Vec<BlockId>,
    suggestions: Vec<BlockId>,
    below: Vec<BlockId>,
}

impl OutputSnapshot {
    /// Returns the number of blocks retained by this presentation model.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Returns the ordered block ids currently present in the suggestions zone.
    pub fn suggestion_ids(&self) -> &[BlockId] {
        &self.suggestions
    }

    /// Allocates and stores a block in this output snapshot.
    pub fn new_block(
        &mut self,
        debug_id: impl Into<String>,
        block: impl Into<StyledBlock>,
    ) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.blocks.insert(id, block.into());
        self.block_debug_ids.insert(id, debug_id.into());
        id
    }

    /// Replaces one block in this output snapshot.
    pub fn set_block(&mut self, id: BlockId, block: impl Into<StyledBlock>) {
        self.blocks.insert(id, block.into());
        self.block_debug_ids
            .entry(id)
            .or_insert_with(|| format!("set-block-{}", id.0));
    }

    /// Removes one block and all of its zone references.
    pub fn remove_block(&mut self, id: BlockId) {
        self.blocks.remove(&id);
        self.block_debug_ids.remove(&id);
        remove_all_from_zone(&mut self.history, id);
        remove_all_from_zone(&mut self.above_active, id);
        remove_all_from_zone(&mut self.above_sticky, id);
        remove_all_from_zone(&mut self.suggestions, id);
        remove_all_from_zone(&mut self.below, id);
    }

    /// Appends a block to snapshot history.
    pub fn push_history(&mut self, id: BlockId) {
        self.history.push(id);
    }

    /// Appends a block to the snapshot active zone.
    pub fn push_above_active(&mut self, id: BlockId) {
        if !self.above_active.contains(&id) {
            self.above_active.push(id);
        }
    }

    /// Moves a block before the first matching snapshot active-zone anchor.
    pub fn push_above_active_before_any<I>(&mut self, id: BlockId, anchors: I)
    where
        I: IntoIterator<Item = BlockId>,
    {
        let anchors = anchors.into_iter().collect::<HashSet<_>>();
        self.above_active.retain(|active_id| *active_id != id);
        let insert_at = self
            .above_active
            .iter()
            .position(|active_id| anchors.contains(active_id))
            .unwrap_or(self.above_active.len());
        self.above_active.insert(insert_at, id);
    }

    /// Appends a block to the snapshot sticky zone.
    pub fn push_above_sticky(&mut self, id: BlockId) {
        if !self.above_sticky.contains(&id) {
            self.above_sticky.push(id);
        }
    }

    /// Removes a block reference from the snapshot sticky zone.
    pub fn remove_above_sticky(&mut self, id: BlockId) {
        self.above_sticky.retain(|block_id| *block_id != id);
    }

    /// Appends a block to the snapshot below-prompt zone.
    pub fn push_below(&mut self, id: BlockId) {
        if !self.below.contains(&id) {
            self.below.push(id);
        }
    }

    /// Creates and appends one snapshot history block.
    pub fn print_output(
        &mut self,
        debug_id: impl Into<String>,
        block: impl Into<StyledBlock>,
    ) -> BlockId {
        let id = self.new_block(debug_id, block);
        self.push_history(id);
        id
    }
}

/// A cloneable handle for mutating prompt zones from any thread.
///
/// Setters update the shared state but do **not** trigger a redraw.
/// Call [`redraw`](TermHandle::redraw) after making all changes.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    output_transaction: Arc<Mutex<()>>,
    sync_condvar: Arc<std::sync::Condvar>,
    redraw: tau_blocking_notify_channel::Sender,
    input_tx: path_std_sync::mpsc::Sender<InputMessage>,
    /// Number of transcript-sized output snapshot clones requested.
    output_snapshot_count: Arc<path_std_sync::atomic::AtomicU64>,
    /// Number of transcript-sized output snapshots transferred by ownership.
    output_snapshot_take_count: Arc<path_std_sync::atomic::AtomicU64>,
    /// Number of asynchronous redraw requests made through this handle in
    /// redraw-count tests.
    #[cfg(feature = "redraw-test-counter")]
    redraw_request_count: Arc<path_std_sync::atomic::AtomicU64>,
}

thread_local! {
    static HELD_OUTPUT_TRANSACTIONS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
}

struct OutputTransactionDepthGuard {
    key: usize,
}

impl Drop for OutputTransactionDepthGuard {
    fn drop(&mut self) {
        HELD_OUTPUT_TRANSACTIONS.with(|held| {
            let mut held = held.borrow_mut();
            let depth = held
                .get_mut(&self.key)
                .expect("output transaction depth missing");
            *depth -= 1;
            if *depth == 0 {
                held.remove(&self.key);
            }
        });
    }
}

/// Guard that serializes terminal output snapshot mutations.
struct OutputTransactionGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    _depth: OutputTransactionDepthGuard,
    /// Monotonic acquisition time used for content-free hold diagnostics.
    acquired_at: std::time::Instant,
}

impl Drop for OutputTransactionGuard<'_> {
    fn drop(&mut self) {
        let held = self.acquired_at.elapsed();
        if Duration::from_millis(500) <= held && admit_stall_warning() {
            tracing::warn!(
                target: "tau_cli_term_raw::frontend_progress",
                hold_ms = held.as_millis(),
                "terminal output transaction stalled"
            );
        }
    }
}

struct RedrawSuppressionGuard<'a> {
    handle: &'a TermHandle,
}

impl<'a> RedrawSuppressionGuard<'a> {
    fn new(handle: &'a TermHandle) -> Self {
        {
            let mut st = handle.lock();
            st.terminal.redraw_suppression = st.terminal.redraw_suppression.saturating_add(1);
        }
        Self { handle }
    }
}

impl Drop for RedrawSuppressionGuard<'_> {
    fn drop(&mut self) {
        let notify = {
            let mut st = self.handle.lock();
            st.terminal.redraw_suppression = st.terminal.redraw_suppression.saturating_sub(1);
            if st.terminal.redraw_suppression == 0 && st.terminal.redraw_dirty_while_suppressed {
                st.terminal.redraw_dirty_while_suppressed = false;
                true
            } else {
                false
            }
        };
        if notify {
            self.handle.redraw.notify();
        }
    }
}

impl TermHandle {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("term state mutex poisoned")
    }

    fn output_transaction_key(&self) -> usize {
        Arc::as_ptr(&self.output_transaction) as usize
    }

    fn output_transaction_is_held(&self) -> bool {
        let key = self.output_transaction_key();
        HELD_OUTPUT_TRANSACTIONS.with(|held| held.borrow().contains_key(&key))
    }

    fn mark_output_transaction_held(&self) -> OutputTransactionDepthGuard {
        let key = self.output_transaction_key();
        HELD_OUTPUT_TRANSACTIONS.with(|held| {
            let mut held = held.borrow_mut();
            *held.entry(key).or_insert(0) += 1;
        });
        OutputTransactionDepthGuard { key }
    }

    fn output_transaction_barrier(&self) -> Option<OutputTransactionGuard<'_>> {
        if self.output_transaction_is_held() {
            return None;
        }
        let waiting_at = path_std_time::Instant::now();
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            "terminal output transaction acquisition started"
        );
        let guard = self
            .output_transaction
            .lock()
            .expect("term output transaction mutex poisoned");
        let waited = waiting_at.elapsed();
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            wait_us = waited.as_micros(),
            "terminal output transaction acquired"
        );
        if Duration::from_millis(500) <= waited && admit_stall_warning() {
            tracing::warn!(
                target: "tau_cli_term_raw::frontend_progress",
                wait_ms = waited.as_millis(),
                "terminal output transaction acquisition stalled"
            );
        }
        let depth = self.mark_output_transaction_held();
        Some(OutputTransactionGuard {
            _guard: guard,
            _depth: depth,
            acquired_at: path_std_time::Instant::now(),
        })
    }

    fn request_redraw_locked(st: &mut SharedState) -> bool {
        if st.terminal.redraw_suppression == 0 {
            true
        } else {
            st.terminal.redraw_dirty_while_suppressed = true;
            false
        }
    }

    fn notify_redraw(&self) {
        #[cfg(feature = "redraw-test-counter")]
        self.redraw_request_count
            .fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
        let notify = {
            let mut st = self.lock();
            Self::request_redraw_locked(&mut st)
        };
        if notify {
            self.redraw.notify();
        }
    }

    /// Requests that the prompt input loop stop and return EOF.
    ///
    /// The input loop waits on an internal channel rather than polling, so a
    /// shutdown message is sent after the shared flag is set to wake any
    /// blocked receiver immediately. Blocking crossterm reads that are
    /// already in flight may finish later; their events are ignored after
    /// this flag is set.
    pub fn request_input_shutdown(&self) {
        self.lock().terminal.input_shutdown = true;
        let _ = self.input_tx.send(InputMessage::Shutdown);
    }

    /// Run `f` while redraw notifications from this handle are suppressed.
    ///
    /// Mutations remain visible in shared state, but redraw requests are marked
    /// dirty and coalesced into one notification after the outermost nested
    /// suppression scope exits. Use this to publish related visible-state
    /// changes as one coherent rendered frame.
    pub fn with_redraw_suppressed<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = RedrawSuppressionGuard::new(self);
        f()
    }

    /// Run `f` while terminal output snapshot mutations from other threads are
    /// blocked.
    ///
    /// This transaction is intentionally narrower than the shared
    /// terminal-state mutex: callers can perform a multi-step snapshot swap
    /// using ordinary [`TermHandle`] methods without exposing the temporary
    /// snapshot to local output producers that own only a cloned handle.
    pub fn with_output_transaction<R>(&self, f: impl FnOnce() -> R) -> R {
        let _transaction = self.output_transaction_barrier();
        f()
    }

    /// Triggers a redraw of the terminal.
    ///
    /// Call this after updating one or more blocks/zones. Multiple
    /// calls coalesce into a single repaint.
    ///
    /// This goes through the differential update path — only the
    /// visible viewport is repainted. Use it for any mutation
    /// guaranteed to be inside the viewport (input, status chip,
    /// streaming live blocks, newly-printed blocks). For mutations
    /// to past blocks that may have scrolled into scrollback, use
    /// [`invalidate_screen`](Self::invalidate_screen) instead. See
    /// `README.md` § "When mutations need a full redraw" for the
    /// full rule.
    pub fn redraw(&self) {
        self.notify_redraw();
    }

    /// Registers one completed selected-transcript mutation for flush
    /// correlation.
    ///
    /// The caller must establish raw frontend-progress TRACE interest before
    /// calling. The delivery identity and caller-owned opaque label remain
    /// process-local and content-free. A caller whose typed opaque fact
    /// invalidates a visible predecessor must suppress redraw
    /// capture across both its presentation mutation and this registration.
    /// Returns `true` exactly when redraw capture was suppressed while the
    /// registration held shared terminal state; `false` means capture was not
    /// suppressed. The return value does not report notification delivery or
    /// eventual writer success.
    pub fn observe_presentation_mutation(
        &self,
        delivery_id: RendererDeliveryId,
        fact: OpaquePresentationFact,
    ) -> bool {
        self.observe_presentation_mutation_enabled(delivery_id, fact)
    }

    /// Registers a fact after the caller has established trace interest.
    fn observe_presentation_mutation_enabled(
        &self,
        delivery_id: RendererDeliveryId,
        fact: OpaquePresentationFact,
    ) -> bool {
        let observed_at = path_std_time::Instant::now();
        let (notify, capture_suppressed) = {
            let mut st = self.lock();
            st.presentation_observations
                .register(delivery_id, fact, observed_at);
            (
                Self::request_redraw_locked(&mut st),
                st.terminal.redraw_suppression != 0,
            )
        };
        if notify {
            self.redraw.notify();
        }
        capture_suppressed
    }

    /// Registers a presentation fact without requiring a global test
    /// subscriber.
    #[cfg(test)]
    fn observe_presentation_mutation_for_test(
        &self,
        delivery_id: RendererDeliveryId,
        fact: OpaquePresentationFact,
    ) -> bool {
        self.observe_presentation_mutation_enabled(delivery_id, fact)
    }

    /// Returns how many asynchronous redraw requests this handle has made.
    ///
    /// This excludes synchronous redraw barriers, which directly notify the
    /// renderer so callers can wait for their completion.
    #[cfg(feature = "redraw-test-counter")]
    pub fn redraw_request_count(&self) -> u64 {
        self.redraw_request_count
            .load(path_std_sync_atomic::Ordering::Relaxed)
    }

    /// Drops every rendered block from every output zone and forces a
    /// full repaint. The prompt, current input buffer, and input-line
    /// history are left intact.
    pub fn clear_output(&self) {
        self.replace_output_snapshot(OutputSnapshot::default());
    }

    /// Returns a clone of all output blocks/zones, excluding prompt input and
    /// prompt-history state.
    pub fn output_snapshot(&self) -> OutputSnapshot {
        self.output_snapshot_count
            .fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
        let _transaction = self.output_transaction_barrier();
        let st = self.lock();
        OutputSnapshot {
            blocks: st.layout.blocks.clone(),
            block_debug_ids: st.layout.block_debug_ids.clone(),
            next_id: st.layout.next_id,
            history: st.layout.history.clone(),
            above_active: st.layout.above_active.clone(),
            above_sticky: st.layout.above_sticky.clone(),
            suggestions: st.layout.suggestions.clone(),
            below: st.layout.below.clone(),
        }
    }

    /// Returns how many full terminal output snapshots this handle has cloned.
    ///
    /// This content-free counter supports frontend progress diagnostics and
    /// guards hidden-agent rendering against transcript-sized clone
    /// regressions.
    pub fn output_snapshot_count(&self) -> u64 {
        self.output_snapshot_count
            .load(path_std_sync_atomic::Ordering::Relaxed)
    }

    /// Transfers all output blocks and zones out of the visible terminal.
    ///
    /// The returned snapshot owns the exact map and zone allocations formerly
    /// installed in the terminal. Prompt input and prompt history remain in the
    /// terminal. Callers must install another snapshot before allowing visible
    /// output mutations.
    pub fn take_output_snapshot(&self) -> OutputSnapshot {
        self.output_snapshot_take_count
            .fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        OutputSnapshot {
            blocks: std::mem::take(&mut st.layout.blocks),
            block_debug_ids: std::mem::take(&mut st.layout.block_debug_ids),
            next_id: st.layout.next_id,
            history: std::mem::take(&mut st.layout.history),
            above_active: std::mem::take(&mut st.layout.above_active),
            above_sticky: std::mem::take(&mut st.layout.above_sticky),
            suggestions: std::mem::take(&mut st.layout.suggestions),
            below: std::mem::take(&mut st.layout.below),
        }
    }

    /// Returns how many output snapshots this handle has transferred by
    /// ownership rather than cloned.
    ///
    /// This content-free counter distinguishes selection handoffs from
    /// transcript-sized clone requests in frontend progress diagnostics.
    pub fn output_snapshot_take_count(&self) -> u64 {
        self.output_snapshot_take_count
            .load(path_std_sync_atomic::Ordering::Relaxed)
    }

    /// Replaces all output blocks/zones, preserving prompt input and history.
    pub fn replace_output_snapshot(&self, snapshot: OutputSnapshot) {
        self.replace_output_snapshot_inner(snapshot, true, true);
    }

    /// Replaces all output blocks/zones without invalidating or redrawing.
    /// The caller must ensure the visible terminal still corresponds to the
    /// restored snapshot.
    pub fn replace_output_snapshot_quiet(&self, snapshot: OutputSnapshot) {
        self.replace_output_snapshot_inner(snapshot, false, false);
    }

    fn replace_output_snapshot_inner(
        &self,
        snapshot: OutputSnapshot,
        invalidate_screen: bool,
        notify: bool,
    ) {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        st.layout.blocks = snapshot.blocks;
        st.layout.block_debug_ids = snapshot.block_debug_ids;
        st.layout.next_id = st.layout.next_id.max(snapshot.next_id);
        st.layout.history = snapshot.history;
        st.rebuild_history_refs();
        st.layout.above_active = snapshot.above_active;
        st.layout.above_sticky = snapshot.above_sticky;
        st.layout.suggestions = snapshot.suggestions;
        st.layout.below = snapshot.below;
        if invalidate_screen {
            st.terminal.invalidate_screen = true;
        }
        let notify = notify && Self::request_redraw_locked(&mut st);
        drop(st);
        if notify {
            self.redraw.notify();
        }
    }

    /// Forces the next redraw to take the full-render path: clear
    /// the visible screen + scrollback (`\x1b[2J\x1b[H\x1b[3J`)
    /// and re-emit the configured suffix of rendered history/log rows plus the
    /// fixed tail. Overflow naturally rebuilds recent terminal scrollback, but
    /// full-redraw plans intentionally omit rubber.
    ///
    /// Use this when a mutation affects rows that may already be in
    /// terminal scrollback — e.g. toggling visibility of a block from
    /// a past turn (`:set show-diff`, `:set show-thinking`). The
    /// differential renderer only repaints the visible window, so
    /// without invalidation those scrolled-out rows would remain as
    /// stale fossils that disagree with current state. See
    /// `README.md` § "When mutations need a full redraw".
    pub fn invalidate_screen(&self) {
        let _transaction = self.output_transaction_barrier();
        self.lock().terminal.invalidate_screen = true;
        self.notify_redraw();
    }

    /// Current terminal size tracked by the renderer.
    pub fn size(&self) -> (usize, usize) {
        let st = self.lock();
        (st.terminal.width, st.terminal.height)
    }

    /// Current terminal height tracked by the renderer.
    pub fn height(&self) -> usize {
        self.lock().terminal.height
    }

    /// Number of full renders performed by the redraw thread since
    /// terminal creation. Temporary debugging aid for scrollback bugs.
    pub fn full_render_count(&self) -> u64 {
        self.lock().terminal.full_render_count
    }

    /// Maximum number of rendered history/log rows replayed during a full
    /// redraw. `usize::MAX` preserves the historical unbounded behavior.
    pub fn redraw_history_size(&self) -> usize {
        self.lock().terminal.redraw_history_size
    }

    /// Updates the maximum number of rendered history/log rows replayed during
    /// full redraw. This method only stores the value; callers decide whether
    /// to invalidate the screen immediately.
    pub fn set_redraw_history_size(&self, redraw_history_size: usize) {
        self.lock().terminal.redraw_history_size = redraw_history_size;
    }

    /// Triggers a redraw and blocks until the redraw thread has
    /// processed it. Uses a generation counter: the caller bumps
    /// `sync_requested`, the redraw thread sets `sync_completed`
    /// atomically with going idle (right before blocking on recv).
    ///
    /// After terminal output fail-stop, this returns immediately without
    /// requesting or retrying a redraw. The failed attachment can no longer
    /// promise that any terminal frame was delivered.
    pub fn redraw_sync(&self) {
        let mut st = self.lock();
        if st.terminal.output_failure.is_some() {
            return;
        }
        st.terminal.sync_requested.advance();
        let target = st.terminal.sync_requested;
        drop(st);

        self.redraw.notify();

        let st = self.state.lock().expect("term state mutex poisoned");
        let _st = self
            .sync_condvar
            .wait_while(st, |s| s.terminal.sync_completed < target)
            .expect("term state mutex poisoned");
    }

    // --- Block management ---

    /// Allocates a new [`BlockId`] and stores the block.
    pub fn new_block(&self, debug_id: impl Into<String>, block: impl Into<StyledBlock>) -> BlockId {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        let id = st.alloc_id();
        let debug_id = debug_id.into();
        let block = block.into();
        let content_empty = block.is_empty();
        st.layout.blocks.insert(id, block);
        st.layout.block_debug_ids.insert(id, debug_id.clone());
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, debug_id, content_empty, "new block");
        id
    }

    /// Updates the content of an existing block (or inserts it at the given
    /// id).
    pub fn set_block(&self, id: BlockId, block: impl Into<StyledBlock>) {
        self.set_block_inner(id, block, false);
    }

    /// Updates a block and reports whether an already-rendered reference
    /// changed.
    pub fn set_block_with_presentation_delta(
        &self,
        id: BlockId,
        block: impl Into<StyledBlock>,
    ) -> bool {
        self.set_block_inner(id, block, true)
    }

    /// Applies one block update with optional presentation comparison.
    fn set_block_inner(
        &self,
        id: BlockId,
        block: impl Into<StyledBlock>,
        observe_delta: bool,
    ) -> bool {
        let _transaction = self.output_transaction_barrier();
        let block = block.into();
        let content_empty = block.is_empty();
        let mut st = self.lock();
        let affects_history = st.block_in_history(id);
        let changed = observe_delta && st.layout.blocks.get(&id) != Some(&block);
        let presentation_changed = changed && st.block_is_visible(id);
        st.layout.blocks.insert(id, block);
        st.layout
            .block_debug_ids
            .entry(id)
            .or_insert_with(|| format!("set-block-{}", id.0));
        if affects_history {
            st.mark_history_dirty_from(0);
        }
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, content_empty, "set block");
        presentation_changed
    }

    /// Removes a block from the central store and every zone that references
    /// it.
    pub fn remove_block(&self, id: BlockId) {
        self.remove_block_inner(id, false);
    }

    /// Removes a block and reports whether any rendered zone referenced it.
    pub fn remove_block_with_presentation_delta(&self, id: BlockId) -> bool {
        self.remove_block_inner(id, true)
    }

    /// Applies one removal with optional presentation inspection.
    fn remove_block_inner(&self, id: BlockId, observe_delta: bool) -> bool {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        st.remove_block(id, observe_delta)
    }

    // --- Zone lists ---

    /// Appends a block id to the history (persistent output).
    pub fn push_history(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        st.append_history(id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "history", "push block zone");
    }

    /// Appends a block id to the above-active zone (if not already
    /// present).
    pub fn push_above_active(&self, id: BlockId) {
        self.push_above_active_inner(id);
    }

    /// Adds an active block and reports whether its rendered zone changed.
    pub fn push_above_active_with_presentation_delta(&self, id: BlockId) -> bool {
        self.push_above_active_inner(id)
    }

    /// Applies one active-zone insertion and reports its exact delta.
    fn push_above_active_inner(&self, id: BlockId) -> bool {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        if !st.layout.above_active.contains(&id) {
            st.layout.above_active.push(id);
            tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "above_active", "push block zone");
            true
        } else {
            false
        }
    }

    /// Inserts a block id into the above-active zone before the first matching
    /// anchor block, or appends it when none of the anchors are active.
    ///
    /// Existing references to `id` are moved rather than duplicated. This keeps
    /// callers from rebuilding the whole output snapshot when they need a
    /// stable sub-order inside the bottom-anchored live block area.
    pub fn push_above_active_before_any<I>(&self, id: BlockId, anchors: I)
    where
        I: IntoIterator<Item = BlockId>,
    {
        self.push_above_active_before_any_inner(id, anchors, false);
    }

    /// Reorders an active block and reports whether the rendered order changed.
    pub fn push_above_active_before_any_with_presentation_delta<I>(
        &self,
        id: BlockId,
        anchors: I,
    ) -> bool
    where
        I: IntoIterator<Item = BlockId>,
    {
        self.push_above_active_before_any_inner(id, anchors, true)
    }

    /// Applies one active-zone reorder with optional comparison.
    fn push_above_active_before_any_inner<I>(
        &self,
        id: BlockId,
        anchors: I,
        observe_delta: bool,
    ) -> bool
    where
        I: IntoIterator<Item = BlockId>,
    {
        let _transaction = self.output_transaction_barrier();
        let anchors = anchors.into_iter().collect::<HashSet<_>>();
        let mut st = self.lock();
        let previous = observe_delta.then(|| st.layout.above_active.clone());
        st.layout.above_active.retain(|&x| x != id);
        let insert_at = st
            .layout
            .above_active
            .iter()
            .position(|active_id| anchors.contains(active_id))
            .unwrap_or(st.layout.above_active.len());
        st.layout.above_active.insert(insert_at, id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "above_active", "insert block zone");
        previous.is_some_and(|previous| st.layout.above_active != previous)
    }

    /// Removes a block id from the above-active zone.
    pub fn remove_above_active(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        self.lock().layout.above_active.retain(|&x| x != id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "above_active", "remove block zone");
    }

    /// Appends a block id to the above-sticky zone (if not already
    /// present).
    pub fn push_above_sticky(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        if !st.layout.above_sticky.contains(&id) {
            st.layout.above_sticky.push(id);
            tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "above_sticky", "push block zone");
        }
    }

    /// Removes a block id from the above-sticky zone.
    pub fn remove_above_sticky(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        self.lock().layout.above_sticky.retain(|&x| x != id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "above_sticky", "remove block zone");
    }

    /// Appends a block id to the suggestions zone (if not already
    /// present). Rendered between the prompt and below blocks.
    pub fn push_suggestions(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        if !st.layout.suggestions.contains(&id) {
            st.layout.suggestions.push(id);
            tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "suggestions", "push block zone");
        }
    }

    /// Removes a block id from the suggestions zone.
    pub fn remove_suggestions(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        self.lock().layout.suggestions.retain(|&x| x != id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "suggestions", "remove block zone");
    }

    /// Appends a block id to the below zone (if not already present).
    pub fn push_below(&self, id: BlockId) {
        self.push_below_inner(id);
    }

    /// Adds a below-prompt block and reports whether its rendered zone changed.
    pub fn push_below_with_presentation_delta(&self, id: BlockId) -> bool {
        self.push_below_inner(id)
    }

    /// Applies one below-zone insertion and reports its exact delta.
    fn push_below_inner(&self, id: BlockId) -> bool {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        if !st.layout.below.contains(&id) {
            st.layout.below.push(id);
            tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "below", "push block zone");
            true
        } else {
            false
        }
    }

    /// Removes a block id from the below zone.
    pub fn remove_below(&self, id: BlockId) {
        let _transaction = self.output_transaction_barrier();
        self.lock().layout.below.retain(|&x| x != id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, zone = "below", "remove block zone");
    }

    // --- Convenience ---

    /// Creates a new block and appends it to the history.
    /// Triggers a redraw automatically.
    pub fn print_output(
        &self,
        debug_id: impl Into<String>,
        block: impl Into<StyledBlock>,
    ) -> BlockId {
        let _transaction = self.output_transaction_barrier();
        let mut st = self.lock();
        let id = st.alloc_id();
        let debug_id = debug_id.into();
        let block = block.into();
        let content_empty = block.is_empty();
        st.layout.blocks.insert(id, block);
        st.layout.block_debug_ids.insert(id, debug_id.clone());
        st.append_history(id);
        tracing::trace!(target: "tau_cli_term_raw::blocks", ?id, debug_id, content_empty, zone = "history", "print output");
        let notify = Self::request_redraw_locked(&mut st);
        drop(st);
        if notify {
            self.redraw.notify();
        }
        id
    }

    /// Updates the left prompt prefix.
    pub fn set_left_prompt(&self, text: impl Into<StyledText>) {
        let mut st = self.lock();
        st.editor.left_prompt = text.into();
        st.ensure_input_cursor_visible();
    }

    /// Returns a clone of the current input buffer.
    pub fn get_buffer(&self) -> String {
        self.lock().editor.buffer.clone()
    }

    /// Returns the current cursor position in bytes.
    pub fn get_cursor(&self) -> usize {
        self.lock().editor.cursor
    }

    /// Replaces the input buffer and cursor position. Also clears
    /// any active history-navigation, completion menu, and prompt undo
    /// state — an external buffer set is treated as a fresh starting
    /// point.
    pub fn set_buffer(&self, text: String, cursor: usize) {
        let mut st = self.lock();
        let new_cursor = clamp_cursor_to_grapheme_boundary(&text, cursor);
        st.editor.buffer = text;
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.editor.completion = None;
        st.editor.current_undo.clear();
        st.editor.current_redo.clear();
        st.write_cursor(new_cursor);
        if abandoned_history_nav {
            st.limit_input_history();
        }
    }

    /// Recalls a queued prompt before the current draft, matching
    /// prompt-history navigation so pressing Down restores the draft that
    /// was present at recall time.
    pub fn recall_prompt_before_current(&self, text: String) {
        let mut st = self.lock();
        st.recall_prompt_before_current(text);
    }

    /// Replaces the input buffer and cursor position without clearing
    /// prompt undo history.
    ///
    /// Use this after the caller has explicitly recorded the current
    /// prompt as an undo snapshot before launching an external picker.
    /// Active history navigation and completion are still closed because
    /// the replacement becomes the new editable draft.
    pub fn set_buffer_preserving_undo(&self, text: String, cursor: usize) {
        let mut st = self.lock();
        let new_cursor = clamp_cursor_to_grapheme_boundary(&text, cursor);
        st.editor.buffer = text;
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.editor.completion = None;
        st.editor.current_redo.clear();
        st.write_cursor(new_cursor);
        if abandoned_history_nav {
            st.limit_input_history();
        }
    }

    /// Snapshot of the open completion menu, if any. Returns `None`
    /// when no menu is showing.
    pub fn completion_state(&self) -> Option<CompletionView> {
        let st = self.lock();
        st.editor.completion.as_ref().map(|c| CompletionView {
            candidates: c.candidates.clone(),
            selected: c.selected,
        })
    }

    /// Updates the right prompt.
    pub fn set_right_prompt(&self, text: impl Into<StyledText>) {
        self.lock().editor.right_prompt = text.into();
    }

    /// Updates the placeholder shown when the input buffer is empty.
    pub fn set_input_placeholder(&self, text: impl Into<StyledText>) {
        self.lock().editor.input_placeholder = text.into();
    }

    /// Enables or disables the compact hidden-row indicator for capped prompt
    /// input.
    pub fn set_prompt_scroll_indicator(&self, enabled: bool) {
        let mut st = self.lock();
        st.editor.show_prompt_scroll_indicator = enabled;
        st.ensure_input_cursor_visible();
    }

    /// Queues a terminal bell to be written by the redraw thread on its next
    /// pass. Goes through the redraw loop so the byte never interleaves with an
    /// in-flight frame.
    pub fn print_terminal_bell(&self) {
        self.queue_terminal_side_effect("\x07");
    }

    /// Queues an iTerm2 OSC 1337 `SetUserVar` side effect.
    ///
    /// `name` must be non-empty printable ASCII, must not contain `=`, and must
    /// be at most 128 bytes. Invalid names are skipped and logged without
    /// echoing the invalid bytes. When `in_tmux` is true, the OSC is wrapped in
    /// a tmux passthrough DCS sequence so the outer terminal receives it.
    ///
    /// `value` is base64-encoded before being written. Invalid `name` values
    /// are rejected and logged rather than emitted because OSC names are
    /// structural escape-sequence fields.
    pub fn print_osc1337_set_user_var(&self, name: &str, value: &str, in_tmux: bool) {
        if let Err(error) = validate_osc1337_name(name) {
            tracing::warn!(
                target: "tau_cli_term_raw::terminal_side_effect",
                name_len = name.len(),
                error,
                "skipping invalid OSC 1337 SetUserVar side effect"
            );
            return;
        }
        let encoded = {
            use base64::Engine as _;
            path_base64_engine::general_purpose::STANDARD.encode(value.as_bytes())
        };
        let sequence = if in_tmux {
            format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar={name}={encoded}\x07\x1b\\")
        } else {
            format!("\x1b]1337;SetUserVar={name}={encoded}\x07")
        };
        self.queue_terminal_side_effect(sequence);
    }

    fn queue_terminal_side_effect(&self, sequence: impl Into<String>) {
        let notify = {
            let mut st = self.lock();
            st.terminal.pending_raw.push(sequence.into());
            Self::request_redraw_locked(&mut st)
        };
        if notify {
            self.redraw.notify();
        }
    }
}

fn validate_osc1337_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("name must not be empty");
    }
    if name.len() > 128 {
        return Err("name must be at most 128 bytes");
    }
    if !name
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b) && b != b'=')
    {
        return Err("name must be printable ASCII without '='");
    }
    Ok(())
}

/// Raw terminal events from crossterm or a virtual test input channel.
pub enum RawEvent {
    /// A decoded key press from crossterm.
    Key(KeyEvent),
    /// Terminal resize event with width and height in cells.
    Resize(u16, u16),
    /// Terminal focus changed.
    FocusChanged {
        /// True when focus was gained; false when it was lost.
        focused: bool,
    },
    /// One bracketed paste. The whole pasted string is delivered
    /// atomically so a multi-line paste doesn't trigger Enter on
    /// embedded newlines.
    Paste(String),
}

enum InputMessage {
    /// A raw terminal event to process unless sticky shutdown/EOF already won.
    Raw(RawEvent),
    /// Wake the receiver and transition it to sticky EOF.
    Shutdown,
    /// A crossterm read error to surface unless shutdown/EOF already won.
    Error(io::Error),
}

/// The first reported output failure that permanently stops one terminal
/// attachment.
#[derive(Clone, Debug)]
struct OutputFailure {
    /// Original standard I/O error classification.
    kind: io::ErrorKind,
    /// Stable error text retained after the originating error is consumed.
    message: String,
}

impl OutputFailure {
    /// Captures an output error for later delivery to the input owner.
    fn new(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    /// Reconstructs an I/O error carrying the attachment-failure marker.
    fn io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.clone())
    }
}

impl std::fmt::Display for OutputFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "terminal output failed: {}", self.message)
    }
}

impl std::error::Error for OutputFailure {}

/// Returns whether an I/O error reports fail-stop of this terminal attachment.
#[must_use]
pub fn is_output_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<OutputFailure>())
}

/// The terminal prompt engine.
///
/// Owns the input event loop. Call [`Term::get_next_event`] in a loop to
/// drive it.
///
/// Real terminals isolate each blocking crossterm read in a short-lived helper
/// thread and deliver the result through an internal channel. This lets
/// shutdown wake the downstream input loop without timeout polling while still
/// avoiding a persistent stdin reader that could race a foreground program such
/// as `$EDITOR`.
///
/// Virtual terminals (tests) use the injected channel branch.
pub struct Term {
    /// Cloneable handle exposing zone/buffer mutators. `Term` derefs
    /// to this so callers can use `term.print_output(...)` etc.
    /// without going through an explicit `.handle()` accessor.
    handle: TermHandle,
    /// Receives raw input, read errors, and shutdown wakeups.
    input_rx: path_std_sync::mpsc::Receiver<InputMessage>,
    /// Redraw thread handle — taken and joined on drop.
    redraw_thread: Option<JoinHandle<()>>,
    /// Whether to disable raw mode on drop (false for virtual terms).
    owns_raw_mode: bool,
    /// Immutable terminal behavior selected before raw mode was acquired.
    terminal_options: TerminalOptions,
    /// Plugged in by callers that want completion. When `None`, the
    /// completion menu never opens; Tab/Esc are no-ops.
    completion_source: Option<Box<dyn CompletionSource>>,
    /// Plugged in by callers that want prompt key bindings.
    bindings: HashMap<KeyBinding, String>,
    /// Whether a high-level owner will finalize each submitted entry before
    /// its raw history can evict older entries.
    defer_submitted_input_history_limit: bool,
}

impl std::ops::Deref for Term {
    type Target = TermHandle;
    fn deref(&self) -> &TermHandle {
        &self.handle
    }
}

impl Term {
    /// Creates a new terminal prompt.
    ///
    /// Enters raw mode with `terminal_options` and spawns the redraw thread.
    /// Returns the prompt engine and a cloneable [`TermHandle`].
    ///
    /// # Errors
    ///
    /// Returns terminal I/O errors from enabling raw mode or terminal input
    /// features. If feature setup fails after raw mode was enabled, raw mode is
    /// disabled on a best-effort basis before returning the error.
    pub fn new(
        left_prompt: impl Into<StyledText>,
        terminal_options: TerminalOptions,
    ) -> io::Result<(Self, TermHandle)> {
        let (width, height) = term_size();
        let state = Arc::new(Mutex::new(SharedState::new(
            width,
            height,
            left_prompt.into(),
        )));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();
        let sync_condvar = Arc::new(path_std_sync::Condvar::new());
        let (input_tx, input_rx) = path_std_sync::mpsc::channel();

        terminal::enable_raw_mode()?;
        // Opt into bracketed paste so the terminal wraps pasted content
        // in `ESC[200~` / `ESC[201~` and crossterm surfaces it as one
        // `CtEvent::Paste(String)` instead of a stream of individual
        // KeyEvents (which, without bracketed paste, leaked literal
        // escape-sequence bytes into the input buffer).
        //
        // Also push the kitty keyboard protocol's
        // `DISAMBIGUATE_ESCAPE_CODES` flag so the terminal sends
        // distinct sequences for combos like `Shift+Enter` /
        // `Ctrl+Enter` that vanilla terminals collapse into a bare
        // `\r`. Terminals that don't implement the protocol silently
        // ignore the escape and we keep the legacy behavior.
        if let Err(error) = initialize_terminal_features(
            &mut io::stdout(),
            terminal_options.cursor_shape,
            terminal_options,
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        let redraw_state = Arc::clone(&state);
        let redraw_writer: Box<dyn Write + Send> = Box::new(io::stdout());
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_input_tx = input_tx.clone();
        let redraw_thread = thread::spawn(move || {
            redraw_loop(
                redraw_state,
                redraw_rx,
                redraw_writer,
                redraw_input_tx,
                &redraw_sync_cv,
            );
        });

        let handle = TermHandle {
            state,
            output_transaction: Arc::new(Mutex::new(())),
            sync_condvar,
            redraw: redraw_tx,
            input_tx,
            output_snapshot_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
            output_snapshot_take_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
            #[cfg(feature = "redraw-test-counter")]
            redraw_request_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
        };

        handle.redraw.notify();

        Ok((
            Self {
                handle: handle.clone(),
                input_rx,
                redraw_thread: Some(redraw_thread),
                owns_raw_mode: true,
                terminal_options,
                completion_source: None,
                bindings: HashMap::new(),
                defer_submitted_input_history_limit: false,
            },
            handle,
        ))
    }

    /// Creates a virtual terminal for testing.
    ///
    /// No raw mode, no crossterm input reader. Output goes to the
    /// provided writer (e.g. a pipe). Input is injected via the
    /// returned `Sender<RawEvent>`. Dropping every returned sender closes
    /// virtual input and makes later reads return sticky [`Event::Eof`].
    pub fn new_virtual(
        width: usize,
        height: usize,
        left_prompt: impl Into<StyledText>,
        output: Box<dyn Write + Send>,
        cursor_shape: CursorShape,
    ) -> (Self, TermHandle, path_std_sync::mpsc::Sender<RawEvent>) {
        let state = Arc::new(Mutex::new(SharedState::new(
            width,
            height,
            left_prompt.into(),
        )));

        let (redraw_tx, redraw_rx) = tau_blocking_notify_channel::channel();
        let sync_condvar = Arc::new(path_std_sync::Condvar::new());
        let (input_tx, input_rx) = path_std_sync::mpsc::channel();

        let redraw_state = Arc::clone(&state);
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_input_tx = input_tx.clone();
        let redraw_thread = thread::spawn(move || {
            redraw_loop(
                redraw_state,
                redraw_rx,
                output,
                redraw_input_tx,
                &redraw_sync_cv,
            );
        });

        let (term_input_tx, term_input_rx) = path_std_sync::mpsc::channel();
        let virtual_input_tx = input_tx.clone();
        thread::spawn(move || {
            while let Ok(raw) = term_input_rx.recv() {
                if virtual_input_tx.send(InputMessage::Raw(raw)).is_err() {
                    break;
                }
            }
            let _ = virtual_input_tx.send(InputMessage::Shutdown);
        });

        let handle = TermHandle {
            state,
            output_transaction: Arc::new(Mutex::new(())),
            sync_condvar,
            redraw: redraw_tx,
            input_tx,
            output_snapshot_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
            output_snapshot_take_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
            #[cfg(feature = "redraw-test-counter")]
            redraw_request_count: Arc::new(path_std_sync_atomic::AtomicU64::new(0)),
        };

        handle.redraw.notify();

        let term = Self {
            handle: handle.clone(),
            input_rx,
            redraw_thread: Some(redraw_thread),
            owns_raw_mode: false,
            terminal_options: TerminalOptions {
                cursor_shape,
                ..TerminalOptions::default()
            },
            completion_source: None,
            bindings: HashMap::new(),
            defer_submitted_input_history_limit: false,
        };

        (term, handle, term_input_tx)
    }

    /// Returns a reference to the embedded [`TermHandle`]. Most
    /// callers can simply call handle methods through `Term`'s
    /// `Deref<Target = TermHandle>` instead.
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Defers submitted-input retention until the high-level owner has
    /// canonicalized or redacted the entry.
    pub fn defer_submitted_input_history_limit(&mut self) {
        self.defer_submitted_input_history_limit = true;
    }

    /// Narrows raw-history bytes for a focused cross-crate test.
    #[cfg(feature = "history-retention-test-support")]
    #[doc(hidden)]
    pub fn set_input_history_max_bytes_for_test(&mut self, max_bytes: usize) {
        self.handle.lock().input_history_limit_override = Some(InputHistoryLimits {
            max_entries: INPUT_HISTORY_MAX_ENTRIES,
            max_bytes,
        });
    }

    /// Applies the raw input-history limit after a deferred submitted entry has
    /// reached its final canonical or redacted representation.
    pub fn finalize_submitted_input_history(&mut self) {
        let mut st = self.handle.lock();
        st.limit_input_history();
        st.editor.last_submitted_input_retained = st.editor.input_history.last().is_some();
    }

    /// Blocks until the next meaningful input event.
    ///
    /// Handles key editing internally (insert, delete, cursor movement)
    /// and only surfaces events the downstream cares about. Triggers
    /// a redraw before returning so internal state changes are visible.
    ///
    /// # Errors
    ///
    /// Returns terminal I/O errors from crossterm reading on real terminals.
    /// Both real and virtual terminals return the typed retained output failure
    /// after their redraw owner fail-stops. Virtual terminals otherwise return
    /// EOF when their injected input channel is disconnected.
    pub fn get_next_event(&self) -> io::Result<Event> {
        loop {
            let raw = match self.next_raw()? {
                Some(ev) => ev,
                None => return Ok(Event::Eof),
            };

            match raw {
                RawEvent::Key(key) => {
                    if let Some(event) = self.handle_key(key)? {
                        self.handle.redraw();
                        return Ok(event);
                    }
                    self.handle.redraw();
                }
                RawEvent::Resize(w, h) => {
                    let (width, height) = {
                        let mut st = self.handle.lock();
                        let width = effective_resize_dimension(w, st.terminal.width);
                        let height = effective_resize_dimension(h, st.terminal.height);
                        st.terminal.width = width;
                        st.terminal.height = height;
                        st.ensure_input_cursor_visible();
                        (width, height)
                    };
                    self.handle.redraw();
                    return Ok(Event::Resize {
                        width: size_event_dimension(width),
                        height: size_event_dimension(height),
                    });
                }
                RawEvent::FocusChanged { focused } => {
                    return Ok(Event::FocusChanged { focused });
                }
                RawEvent::Paste(text) => {
                    // Insert the whole paste at the cursor in one go.
                    // Going through the per-char path would re-trigger
                    // the redraw thread N times and, more importantly,
                    // would expose embedded `\n` bytes to the Enter
                    // handler and submit the line mid-paste.
                    if text.is_empty() {
                        self.handle.redraw();
                        continue;
                    }
                    let text = normalize_paste_text(text);
                    {
                        let mut st = self.handle.lock();
                        st.record_undo();
                        let cursor = st.editor.cursor;
                        st.editor.buffer.insert_str(cursor, &text);
                        st.write_cursor(cursor + text.len());
                        st.sync_buffer_to_history_nav();
                    }
                    self.refresh_completion();
                    self.handle.redraw();
                    return Ok(Event::BufferChanged);
                }
            }
        }
    }

    /// Reads the next raw event, blocking until one arrives.
    ///
    /// Real terminals perform the blocking crossterm read in a one-shot helper
    /// thread and wait on the same channel used for shutdown wakeups. The
    /// helper is intentionally not persistent: callers only launch external
    /// programs after `get_next_event` returns, so no reader remains active
    /// to race those programs for stdin. If shutdown wins the race, any
    /// later helper result is dropped by the closed or shutdown channel
    /// path.
    fn next_raw(&self) -> io::Result<Option<RawEvent>> {
        {
            let st = self.handle.lock();
            if let Some(error) = &st.terminal.output_failure {
                return Err(error.io_error());
            }
            if st.terminal.input_shutdown {
                return Ok(None);
            }
        }

        if self.owns_raw_mode {
            let tx = self.handle.input_tx.clone();
            thread::spawn(move || {
                let message = match read_real_raw_event(event::read, raw_term_size) {
                    Ok(raw) => InputMessage::Raw(raw),
                    Err(error) => InputMessage::Error(error),
                };
                let _ = tx.send(message);
            });
        }

        let message = match self.input_rx.recv() {
            Ok(message) => message,
            Err(_) => return Ok(None),
        };
        {
            let st = self.handle.lock();
            if let Some(error) = &st.terminal.output_failure {
                return Err(error.io_error());
            }
            if st.terminal.input_shutdown {
                return Ok(None);
            }
        }
        match message {
            InputMessage::Raw(raw) => Ok(Some(raw)),
            InputMessage::Shutdown => {
                self.handle.lock().terminal.input_shutdown = true;
                Ok(None)
            }
            InputMessage::Error(error) => Err(error),
        }
    }

    /// Plugs in (or replaces) the completion source. Pass `None` to
    /// disable completion entirely. Closes the menu if currently open.
    pub fn set_completion_source(&mut self, source: Option<Box<dyn CompletionSource>>) {
        self.completion_source = source;
        let mut st = self.handle.lock();
        st.editor.completion = None;
    }

    /// Configures key bindings surfaced as [`Event::Binding`].
    ///
    /// Supported key spellings include `Tab`, `BackTab`, `Shift-Tab`, `Enter`,
    /// `Esc`, arrow/navigation/editing keys, `C-Enter`, `C-Up`, `C-Down`, and
    /// `C-<letter>`, and canonical `M-<ascii-character>` for exact Alt-only
    /// character events. Control letters are case-sensitive:
    /// `C-b` and shifted `C-B` may have different actions when the terminal
    /// reports Shift.
    pub fn set_bindings(&mut self, bindings: impl IntoIterator<Item = (String, String)>) {
        self.bindings = bindings
            .into_iter()
            .filter_map(|(raw_key, action)| {
                let parsed = parse_key_binding(&raw_key);
                tracing::trace!(
                    target: "tau_cli_term_raw::input",
                    raw_key,
                    ?parsed,
                    action,
                    "configured prompt binding"
                );
                parsed.map(|key| (key, action))
            })
            .collect();
    }

    /// Appends previously submitted prompts to the input history.
    ///
    /// Intended for startup seeding from persistent history. Empty
    /// prompts are ignored, and the active edit buffer is left intact.
    pub fn seed_input_history(&mut self, history: impl IntoIterator<Item = String>) {
        let mut st = self.handle.lock();
        st.editor.input_history.extend(
            history
                .into_iter()
                .filter(|buffer| !buffer.is_empty())
                .map(PromptDraft::submitted),
        );
        st.limit_input_history();
        st.editor.history_nav = None;
        st.editor.last_submitted_input_retained = false;
    }

    /// Replaces the most recently submitted input-history entry and any
    /// recalled source entry that the submission edited.
    ///
    /// Higher layers use this after canonicalizing prompt syntax that the raw
    /// editor intentionally does not interpret.
    pub fn replace_last_submitted_input(&mut self, text: String) {
        let mut st = self.handle.lock();
        let recalled_source = st.editor.last_submitted_recalled_source;
        if let Some(index) = recalled_source
            && let Some(source) = st.editor.input_history.get_mut(index)
        {
            *source = PromptDraft::submitted(text.clone());
        }
        if st.editor.last_submitted_input_retained {
            if let Some(last) = st.editor.input_history.last_mut() {
                *last = PromptDraft::submitted(text.clone());
            }
        } else if !text.is_empty() {
            st.editor
                .input_history
                .push(PromptDraft::submitted(text.clone()));
        }
        if !self.defer_submitted_input_history_limit {
            st.limit_input_history();
        }
        st.editor.last_submitted_input_retained = st
            .editor
            .input_history
            .last()
            .is_some_and(|draft| draft.buffer == text);
        st.editor.history_nav = None;
        st.editor.completion = None;
    }

    /// Re-evaluates the completion source against the current buffer
    /// and updates the menu state accordingly. Called from buffer
    /// mutation paths (typing, paste, backspace, kill-line, etc.).
    /// Treats every mutation as committing any prior preview: the
    /// new buffer/cursor become the menu's `original_*` so a later
    /// Esc returns here, not to a stale earlier state.
    fn refresh_completion(&self) {
        let Some(source) = self.completion_source.as_deref() else {
            return;
        };
        let (buffer, cursor) = {
            let st = self.handle.lock();
            (st.editor.buffer.clone(), st.editor.cursor)
        };
        let candidates = source
            .candidates(&buffer, cursor)
            .into_iter()
            .filter(|candidate| {
                candidate.cursor
                    == clamp_cursor_to_grapheme_boundary(&candidate.replacement, candidate.cursor)
            })
            .collect::<Vec<_>>();
        let mut st = self.handle.lock();
        if candidates.is_empty() {
            st.editor.completion = None;
        } else {
            st.editor.completion = Some(CompletionMenu {
                candidates,
                selected: None,
                original_buffer: buffer,
                original_cursor: cursor,
            });
        }
    }

    /// Releases the terminal for an external program (e.g. `$EDITOR`):
    /// disables raw mode + bracketed paste, restores the user-configured
    /// cursor shape, and clears the screen so the editor starts on a clean
    /// canvas.
    ///
    /// No reader-thread coordination is needed: the one-shot crossterm reader
    /// is joined logically by `get_next_event` returning before callers can
    /// launch the external program, so no persistent stdin reader remains
    /// active while the program owns the terminal.
    ///
    /// # Errors
    ///
    /// Returns terminal I/O errors from releasing raw-mode features or clearing
    /// the screen. On failure, Tau attempts to roll terminal ownership back via
    /// [`Self::resume_after_external`], which also unmutes redraws and
    /// invalidates the next frame.
    pub fn pause_for_external(&self) -> io::Result<()> {
        if !self.owns_raw_mode {
            return Ok(());
        }
        self.pause_for_external_with_release(|| {
            let mut stdout = io::stdout();
            write_external_pause_features(&mut stdout, self.terminal_options)?;
            terminal::disable_raw_mode()?;
            crossterm::execute!(
                io::stdout(),
                crossterm::style::ResetColor,
                crossterm::cursor::MoveTo(0, 0),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            )?;
            Ok(())
        })
    }

    fn pause_for_external_with_release(
        &self,
        release_terminal: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        {
            let mut st = self.handle.lock();
            st.terminal.external_paused = true;
        }
        // Wait until any redraw frame that already passed the paused-state
        // check has finished writing before releasing the terminal to an
        // external program.
        self.handle.redraw_sync();

        if let Err(error) = release_terminal() {
            let _ = self.resume_after_external();
            return Err(error);
        }
        Ok(())
    }

    /// Re-acquires raw mode + bracketed paste after an external
    /// program. Marks the redraw thread's `Screen` cache stale so the
    /// next render repaints from scratch; without this, the cache
    /// would diff against what we *thought* was on screen and skip
    /// drawing anything since the editor exited.
    ///
    /// # Errors
    ///
    /// Returns terminal I/O errors from re-enabling raw-mode features or
    /// clearing the screen. Even on failure, the redraw pause is cleared, the
    /// tracked terminal size is refreshed, and the next frame is invalidated.
    pub fn resume_after_external(&self) -> io::Result<()> {
        if !self.owns_raw_mode {
            self.finish_external_resume();
            return Ok(());
        }
        let result = (|| -> io::Result<()> {
            terminal::enable_raw_mode()?;
            let mut stdout = io::stdout();
            write_external_resume_features(
                &mut stdout,
                self.terminal_options.cursor_shape,
                self.terminal_options,
            )?;
            crossterm::execute!(
                io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )?;
            Ok(())
        })();
        self.finish_external_resume();
        result
    }

    fn finish_external_resume(&self) {
        let (width, height) = term_size();
        {
            let mut st = self.handle.lock();
            st.terminal.width = width;
            st.terminal.height = height;
            st.ensure_input_cursor_visible();
            st.terminal.external_paused = false;
            st.terminal.invalidate_screen = true;
        }
        self.handle.redraw();
    }

    /// Records the current prompt as an undo snapshot without changing
    /// the visible buffer.
    ///
    /// External pickers call this before releasing the terminal so that
    /// a later undo restores the draft that was on screen when the
    /// picker opened.
    pub fn record_prompt_undo(&self) {
        let mut st = self.handle.lock();
        st.record_undo();
    }

    /// Programmatically inserts a newline into the prompt.
    ///
    /// This is the same editing operation as unbound `Enter`,
    /// `Shift-Enter`, or `Alt-Enter`.
    pub fn trigger_insert_newline(&self) -> Event {
        self.insert_newline()
    }

    /// Programmatically submits the prompt or accepts a completion preview.
    ///
    /// This is the same operation as unbound `Ctrl-Enter`: if a
    /// completion candidate is previewed, it is accepted without
    /// submitting; otherwise the current prompt is submitted.
    pub fn trigger_submit_or_accept_completion(&self) -> Event {
        self.submit_or_accept_completion()
    }

    /// Programmatically closes any open completion menu.
    ///
    /// Returns `true` when a menu was open and got dismissed. If the
    /// selected completion had previewed text in the input buffer, the
    /// buffer is restored to the text that opened the menu.
    pub fn dismiss_completion_menu(&self) -> bool {
        let mut st = self.handle.lock();
        st.dismiss_completion()
    }

    /// Programmatically triggers a history step (the same operation
    /// `Up`/`Down` and `Ctrl-K`/`Ctrl-J` perform). Closes any open
    /// completion menu first so callers don't have to coordinate with
    /// the input loop.
    pub fn trigger_history_step(&self, delta: isize) {
        let mut st = self.handle.lock();
        st.editor.completion = None;
        st.step_history(delta);
    }

    /// Programmatically triggers prompt undo.
    pub fn trigger_undo(&self) -> bool {
        let mut st = self.handle.lock();
        st.editor.completion = None;
        st.undo()
    }

    /// Programmatically triggers prompt redo.
    pub fn trigger_redo(&self) -> bool {
        let mut st = self.handle.lock();
        st.editor.completion = None;
        st.redo()
    }

    fn step_history_event(&self, delta: isize) -> io::Result<Option<Event>> {
        self.trigger_history_step(delta);
        Ok(Some(Event::BufferChanged))
    }

    fn binding_action(&self, binding: &Option<KeyBinding>) -> Option<String> {
        binding
            .as_ref()
            .and_then(|key| self.bindings.get(key))
            .cloned()
    }

    /// Handles keys that belong to an open completion menu before any
    /// configurable binding can match them. Returns `None` when no completion
    /// action applies, letting normal key handling continue.
    fn handle_completion_key(
        &self,
        key: KeyEvent,
        ctrl: bool,
        shift: bool,
        alt: bool,
    ) -> Option<Event> {
        match key.code {
            KeyCode::Tab => {
                let mut st = self.handle.lock();
                st.cycle_completion(1).then_some(Event::BufferChanged)
            }
            KeyCode::BackTab | KeyCode::Up => {
                let mut st = self.handle.lock();
                st.cycle_completion(-1).then_some(Event::BufferChanged)
            }
            KeyCode::Down => {
                let mut st = self.handle.lock();
                st.cycle_completion(1).then_some(Event::BufferChanged)
            }
            KeyCode::Esc => {
                let mut st = self.handle.lock();
                st.dismiss_completion().then_some(Event::BufferChanged)
            }
            KeyCode::Enter if ctrl || (!shift && !alt) => self.accept_completion_event(),
            _ => None,
        }
    }

    fn move_cursor_left(&self) -> bool {
        let mut st = self.handle.lock();
        if st.editor.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&st.editor.buffer, st.editor.cursor);
        st.write_cursor(prev);
        true
    }

    fn move_cursor_right(&self) -> bool {
        let mut st = self.handle.lock();
        if st.editor.buffer.len() <= st.editor.cursor {
            return false;
        }
        let next = next_char_boundary(&st.editor.buffer, st.editor.cursor);
        st.write_cursor(next);
        true
    }

    fn move_cursor_start(&self) -> bool {
        let mut st = self.handle.lock();
        if st.editor.cursor == 0 {
            return false;
        }
        st.write_cursor(0);
        true
    }

    fn move_cursor_end(&self) -> bool {
        let mut st = self.handle.lock();
        let len = st.editor.buffer.len();
        if st.editor.cursor == len {
            return false;
        }
        st.write_cursor(len);
        true
    }

    fn delete_backward(&self) -> bool {
        let changed = {
            let mut st = self.handle.lock();
            if st.editor.cursor == 0 {
                return false;
            }
            st.record_undo();
            let prev = prev_char_boundary(&st.editor.buffer, st.editor.cursor);
            let cursor = st.editor.cursor;
            st.editor.buffer.drain(prev..cursor);
            st.write_cursor(prev);
            st.sync_buffer_to_history_nav();
            true
        };
        self.refresh_completion();
        changed
    }

    fn delete_forward(&self) -> bool {
        let changed = {
            let mut st = self.handle.lock();
            if st.editor.buffer.len() <= st.editor.cursor {
                return false;
            }
            st.record_undo();
            let cursor = st.editor.cursor;
            let next = next_char_boundary(&st.editor.buffer, cursor);
            st.editor.buffer.drain(cursor..next);
            st.write_cursor(cursor);
            st.sync_buffer_to_history_nav();
            true
        };
        self.refresh_completion();
        changed
    }

    fn clear_prompt(&self) -> bool {
        let changed = {
            let mut st = self.handle.lock();
            if st.editor.buffer.is_empty() {
                return false;
            }
            st.editor.ctrl_c_cancel_armed = false;
            st.record_undo();
            st.editor.buffer.clear();
            let abandoned_history_nav = st.editor.history_nav.take().is_some();
            st.editor.completion = None;
            st.write_cursor(0);
            if abandoned_history_nav {
                st.limit_input_history();
            }
            true
        };
        self.refresh_completion();
        changed
    }

    fn clear_or_cancel_prompt(&self) -> Event {
        let mut st = self.handle.lock();
        if st.editor.buffer.is_empty() {
            if st.editor.ctrl_c_cancel_armed {
                st.editor.ctrl_c_cancel_armed = false;
                return Event::CancelPrompt;
            }
            st.editor.ctrl_c_cancel_armed = true;
            return Event::Notice(
                "Press Ctrl-C again to cancel the current response; use Ctrl-D to exit".to_owned(),
            );
        }
        st.editor.ctrl_c_cancel_armed = false;
        st.record_undo();
        st.editor.buffer.clear();
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.editor.completion = None;
        st.write_cursor(0);
        if abandoned_history_nav {
            st.limit_input_history();
        }
        drop(st);
        self.refresh_completion();
        Event::BufferChanged
    }

    fn kill_to_start(&self) -> bool {
        let changed = {
            let mut st = self.handle.lock();
            if st.editor.cursor == 0 {
                return false;
            }
            st.record_undo();
            let cursor = st.editor.cursor;
            st.editor.buffer.drain(..cursor);
            st.write_cursor(0);
            st.sync_buffer_to_history_nav();
            true
        };
        self.refresh_completion();
        changed
    }

    fn kill_word_left(&self) -> bool {
        let changed = {
            let mut st = self.handle.lock();
            if st.editor.cursor == 0 {
                return false;
            }
            let new_end = word_left_boundary(&st.editor.buffer, st.editor.cursor);
            st.record_undo();
            let cursor = st.editor.cursor;
            st.editor.buffer.drain(new_end..cursor);
            st.write_cursor(new_end);
            st.sync_buffer_to_history_nav();
            true
        };
        self.refresh_completion();
        changed
    }

    fn move_cursor_vertical_event(&self, delta: isize) -> Option<Event> {
        let mut st = self.handle.lock();
        let target_col = st.vertical_target_col();
        if let Some(new_cursor) = move_cursor_vertical(&st, delta, target_col) {
            st.write_cursor_keep_sticky(new_cursor);
            return Some(Event::BufferChanged);
        }
        None
    }

    fn cycle_or_move_up(&self) -> Option<Event> {
        let mut st = self.handle.lock();
        if st.cycle_completion(-1) {
            return Some(Event::BufferChanged);
        }
        let target_col = st.vertical_target_col();
        if let Some(new_cursor) = move_cursor_vertical(&st, -1, target_col) {
            st.write_cursor_keep_sticky(new_cursor);
            return Some(Event::BufferChanged);
        }
        if st.step_history(-1) {
            return Some(Event::BufferChanged);
        }
        None
    }

    fn cycle_or_move_down(&self) -> Option<Event> {
        let mut st = self.handle.lock();
        if st.cycle_completion(1) {
            return Some(Event::BufferChanged);
        }
        let target_col = st.vertical_target_col();
        if let Some(new_cursor) = move_cursor_vertical(&st, 1, target_col) {
            st.write_cursor_keep_sticky(new_cursor);
            return Some(Event::BufferChanged);
        }
        if st.step_history(1) {
            return Some(Event::BufferChanged);
        }
        None
    }

    fn cycle_completion_event(&self, delta: isize) -> Option<Event> {
        let mut st = self.handle.lock();
        st.cycle_completion(delta).then_some(Event::BufferChanged)
    }

    fn dismiss_completion_event(&self) -> Option<Event> {
        let mut st = self.handle.lock();
        st.dismiss_completion().then_some(Event::BufferChanged)
    }

    fn accept_completion_event(&self) -> Option<Event> {
        let accepted = {
            let mut st = self.handle.lock();
            st.accept_completion()
        };
        if !accepted {
            return None;
        }
        self.refresh_completion();
        Some(Event::CompletionAccept)
    }

    /// Returns true when `action` is handled by [`Self::trigger_named_action`].
    pub fn is_named_action(action: &str) -> bool {
        named_action_handler(action).is_some()
    }

    /// Runs one named raw prompt action, returning the event it produced.
    ///
    /// These action names make built-in editing and prompt UI behaviors
    /// available to the configurable binding layer.
    pub fn trigger_named_action(&self, action: &str) -> Option<Event> {
        named_action_handler(action).and_then(|handler| handler(self))
    }

    fn backtab_action(&self) -> Option<Event> {
        Some(Event::BackTab)
    }

    fn clear_prompt_action(&self) -> Option<Event> {
        self.clear_prompt().then_some(Event::BufferChanged)
    }

    fn clear_or_cancel_prompt_action(&self) -> Option<Event> {
        Some(self.clear_or_cancel_prompt())
    }

    fn move_cursor_end_action(&self) -> Option<Event> {
        self.move_cursor_end().then_some(Event::BufferChanged)
    }

    fn move_cursor_left_action(&self) -> Option<Event> {
        self.move_cursor_left().then_some(Event::BufferChanged)
    }

    fn move_cursor_right_action(&self) -> Option<Event> {
        self.move_cursor_right().then_some(Event::BufferChanged)
    }

    fn move_cursor_start_action(&self) -> Option<Event> {
        self.move_cursor_start().then_some(Event::BufferChanged)
    }

    fn delete_backward_action(&self) -> Option<Event> {
        self.delete_backward().then_some(Event::BufferChanged)
    }

    fn delete_forward_action(&self) -> Option<Event> {
        self.delete_forward().then_some(Event::BufferChanged)
    }

    fn escape_action(&self) -> Option<Event> {
        Some(Event::Escape)
    }

    fn kill_to_start_action(&self) -> Option<Event> {
        self.kill_to_start().then_some(Event::BufferChanged)
    }

    fn kill_word_left_action(&self) -> Option<Event> {
        self.kill_word_left().then_some(Event::BufferChanged)
    }

    fn move_cursor_down_action(&self) -> Option<Event> {
        self.move_cursor_vertical_event(1)
    }

    fn move_cursor_up_action(&self) -> Option<Event> {
        self.move_cursor_vertical_event(-1)
    }

    fn prompt_eof_action(&self) -> Option<Event> {
        let is_empty = self.handle.lock().editor.buffer.is_empty();
        is_empty.then_some(Event::Eof)
    }

    fn select_completion_next_action(&self) -> Option<Event> {
        self.cycle_completion_event(1)
    }

    fn select_completion_previous_action(&self) -> Option<Event> {
        self.cycle_completion_event(-1)
    }

    fn insert_newline(&self) -> Event {
        {
            let mut st = self.handle.lock();
            st.editor.completion = None;
            st.record_undo();
            let cursor = st.editor.cursor;
            st.editor.buffer.insert(cursor, '\n');
            st.write_cursor(cursor + 1);
            st.sync_buffer_to_history_nav();
        }
        self.refresh_completion();
        Event::BufferChanged
    }

    fn submit_or_accept_completion(&self) -> Event {
        // If a candidate is previewed, accept it but stay on the
        // line — the buffer already reflects the replacement (cycling
        // previewed it), so we just close the menu and surface a
        // distinct event.
        if self.accept_completion_event().is_some() {
            return Event::CompletionAccept;
        }
        let started = path_std_time::Instant::now();
        let line = {
            let mut st = self.handle.lock();
            st.editor.completion = None;
            st.editor.last_submitted_recalled_source =
                st.editor.history_nav.as_ref().and_then(|nav| {
                    nav.entries
                        .get(nav.index)
                        .and_then(|entry| entry.source_index)
                });
            st.editor.history_nav = None;
            let line = st.editor.buffer.clone();
            st.push_current_as_history_entry(!self.defer_submitted_input_history_limit);
            st.editor.last_submitted_input_retained = st
                .editor
                .input_history
                .last()
                .is_some_and(|draft| draft.buffer == line);
            line
        };
        tracing::trace!(
            target: "tau_cli::prompt_submission",
            stage = "raw_submit_clear",
            prompt_bytes = line.len(),
            stage_us = started.elapsed().as_micros(),
            "content-free prompt submission stage"
        );
        Event::Line(line)
    }

    fn handle_enter_key(&self, ctrl: bool, shift: bool, alt: bool) -> Event {
        if shift || alt {
            // Shift+Enter / Alt+Enter keep their explicit newline affordance.
            // This also keeps newline working when a user binds plain Enter to
            // an action.
            // Shift+Enter only reaches us when the terminal stack emits CSI-u
            // format (e.g. `\e[13;2u`): native kitty protocol, fixterms, or
            // tmux 3.5+ with `extended-keys-format csi-u`. Crossterm does NOT
            // parse the xterm modifyOtherKeys CSI-27 form (`\e[27;2;13~`), so
            // tmux configured with `extended-keys-format xterm` will swallow it.
            // Alt+Enter is the universal fallback because every terminal sends
            // `\e\r` for it regardless of protocol negotiation.
            return self.insert_newline();
        }

        if ctrl {
            self.submit_or_accept_completion()
        } else {
            self.insert_newline()
        }
    }

    fn write_cursor_start_raw(&self) {
        let mut st = self.handle.lock();
        st.write_cursor(0);
    }

    fn write_cursor_end_raw(&self) {
        let mut st = self.handle.lock();
        let len = st.editor.buffer.len();
        st.write_cursor(len);
    }

    fn kill_to_start_raw_event(&self) -> Event {
        {
            let mut st = self.handle.lock();
            st.record_undo();
            let cursor = st.editor.cursor;
            st.editor.buffer.drain(..cursor);
            st.write_cursor(0);
            st.sync_buffer_to_history_nav();
        }
        self.refresh_completion();
        Event::BufferChanged
    }

    // Keep Ctrl-C's raw fallback local instead of delegating to
    // `clear_or_cancel_prompt`: this path historically cleared a non-empty
    // prompt without refreshing completions, and callers may observe that exact
    // event/refresh boundary.
    fn handle_ctrl_c_key(&self) -> Event {
        let mut st = self.handle.lock();
        if st.editor.buffer.is_empty() {
            if st.editor.ctrl_c_cancel_armed {
                st.editor.ctrl_c_cancel_armed = false;
                return Event::CancelPrompt;
            }
            st.editor.ctrl_c_cancel_armed = true;
            return Event::Notice(
                "Press Ctrl-C again to cancel the current response; use Ctrl-D to exit".to_owned(),
            );
        }
        st.editor.ctrl_c_cancel_armed = false;
        st.record_undo();
        st.editor.buffer.clear();
        let abandoned_history_nav = st.editor.history_nav.take().is_some();
        st.editor.completion = None;
        st.write_cursor(0);
        if abandoned_history_nav {
            st.limit_input_history();
        }
        Event::BufferChanged
    }

    fn handle_control_char_key(&self, ch: char) -> io::Result<Option<Event>> {
        match ch {
            'd' => {
                let is_empty = self
                    .state
                    .lock()
                    .expect("term state mutex poisoned")
                    .editor
                    .buffer
                    .is_empty();
                Ok(is_empty.then_some(Event::Eof))
            }
            'c' => Ok(Some(self.handle_ctrl_c_key())),
            'u' => Ok(Some(self.kill_to_start_raw_event())),
            'w' => Ok(self.kill_word_left().then_some(Event::BufferChanged)),
            'a' => {
                self.write_cursor_start_raw();
                Ok(None)
            }
            'e' => {
                self.write_cursor_end_raw();
                Ok(None)
            }
            'o' | 'g' => Ok(Some(Event::ExternalEditor)),
            'j' => self.step_history_event(1),
            'k' => self.step_history_event(-1),
            _ => Ok(None),
        }
    }

    fn insert_char_event(&self, ch: char) -> Event {
        {
            let mut st = self.handle.lock();
            st.record_undo();
            let cursor = st.editor.cursor;
            st.editor.buffer.insert(cursor, ch);
            st.write_cursor(cursor + ch.len_utf8());
            st.sync_buffer_to_history_nav();
        }
        self.refresh_completion();
        Event::BufferChanged
    }

    fn handle_plain_edit_key(&self, code: KeyCode) -> Option<Event> {
        match code {
            KeyCode::Backspace => self.delete_backward().then_some(Event::BufferChanged),
            KeyCode::Delete => self.delete_forward().then_some(Event::BufferChanged),
            _ => None,
        }
    }

    fn handle_plain_cursor_key(&self, code: KeyCode) {
        match code {
            KeyCode::Left => {
                self.move_cursor_left();
            }
            KeyCode::Right => {
                self.move_cursor_right();
            }
            KeyCode::Home => {
                self.write_cursor_start_raw();
            }
            KeyCode::End => {
                self.write_cursor_end_raw();
            }
            _ => {}
        }
    }

    fn handle_vertical_key(&self, code: KeyCode, ctrl: bool) -> io::Result<Option<Event>> {
        match (code, ctrl) {
            (KeyCode::Up, true) => self.step_history_event(-1),
            (KeyCode::Down, true) => self.step_history_event(1),
            (KeyCode::Up, false) => Ok(self.cycle_or_move_up()),
            (KeyCode::Down, false) => Ok(self.cycle_or_move_down()),
            _ => Ok(None),
        }
    }

    fn handle_unbound_key(
        &self,
        key: KeyEvent,
        ctrl: bool,
        shift: bool,
        alt: bool,
    ) -> io::Result<Option<Event>> {
        match key.code {
            KeyCode::Enter => Ok(Some(self.handle_enter_key(ctrl, shift, alt))),
            KeyCode::Char(ch) if ctrl => self.handle_control_char_key(ch),
            KeyCode::Char(ch) => Ok(Some(self.insert_char_event(ch))),
            KeyCode::Backspace | KeyCode::Delete => Ok(self.handle_plain_edit_key(key.code)),
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {
                self.handle_plain_cursor_key(key.code);
                Ok(None)
            }
            KeyCode::Up | KeyCode::Down => self.handle_vertical_key(key.code, ctrl),
            KeyCode::BackTab => Ok(Some(Event::BackTab)),
            KeyCode::Esc => Ok(Some(Event::Escape)),
            _ => Ok(None),
        }
    }

    fn handle_key(&self, key: KeyEvent) -> io::Result<Option<Event>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let binding = key_binding_for_event(key, ctrl);
        tracing::trace!(
            target: "tau_cli_term_raw::input",
            ?key,
            ctrl,
            shift,
            alt,
            ?binding,
            binding_count = self.bindings.len(),
            "handling key event"
        );

        let ctrl_c = matches!(key.code, KeyCode::Char('c')) && ctrl;
        if !ctrl_c {
            self.handle.lock().editor.ctrl_c_cancel_armed = false;
        }

        if let Some(event) = self.handle_completion_key(key, ctrl, shift, alt) {
            return Ok(Some(event));
        }

        if let Some(action) = self.binding_action(&binding) {
            tracing::trace!(
                target: "tau_cli_term_raw::input",
                ?binding,
                action,
                "matched configured binding"
            );
            return Ok(Some(Event::Binding(action)));
        }

        self.handle_unbound_key(key, ctrl, shift, alt)
    }
}

impl Term {
    /// Signals the redraw thread to do one final render, reposition
    /// the cursor below all content, and exit. Blocks until complete.
    fn shutdown(&mut self) {
        // Set the flag first, then notify — the redraw thread checks
        // the flag before blocking on recv, so it will see it on the
        // next iteration.
        {
            let mut st = self.handle.lock();
            st.terminal.shutdown = true;
        }
        self.handle.redraw.notify();

        if let Some(handle) = self.redraw_thread.take() {
            let _ = handle.join();
        }
    }
}

fn word_left_boundary(buffer: &str, cursor: usize) -> usize {
    let before_cursor = &buffer[..cursor];
    let trimmed_end = before_cursor.trim_end_matches(char::is_whitespace).len();
    before_cursor[..trimmed_end]
        .char_indices()
        .rev()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index + ch.len_utf8()))
        .unwrap_or(0)
}

fn read_real_raw_event(
    mut read: impl FnMut() -> io::Result<CtEvent>,
    mut term_size: impl FnMut() -> io::Result<(u16, u16)>,
) -> io::Result<RawEvent> {
    loop {
        let raw = read()?;
        tracing::trace!(target: "tau_cli_term_raw::input", ?raw, "terminal raw input event");
        match raw {
            CtEvent::Key(key) => {
                // The kitty protocol surfaces Press/Repeat/Release events; drop
                // Release here so each keystroke fires exactly once downstream.
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                return Ok(RawEvent::Key(key));
            }
            CtEvent::Resize(w, h) => {
                let (actual_w, actual_h) = term_size().unwrap_or((0, 0));
                return Ok(RawEvent::Resize(
                    resample_resize_dimension(w, actual_w),
                    resample_resize_dimension(h, actual_h),
                ));
            }
            CtEvent::FocusGained => return Ok(RawEvent::FocusChanged { focused: true }),
            CtEvent::FocusLost => return Ok(RawEvent::FocusChanged { focused: false }),
            CtEvent::Paste(text) => return Ok(RawEvent::Paste(text)),
            // Mouse events: skip so the caller still observes stdin as
            // "blocking" without unbounded recursion under noisy input.
            _ => {}
        }
    }
}

fn write_external_pause_features(
    writer: &mut impl Write,
    terminal_options: TerminalOptions,
) -> io::Result<()> {
    if !terminal_options.mouse {
        crossterm::execute!(writer, DisableMouseCapture)?;
    }
    crossterm::execute!(
        writer,
        PopKeyboardEnhancementFlags,
        crossterm::event::DisableFocusChange,
        crossterm::event::DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape,
    )
}

fn write_external_resume_features(
    writer: &mut impl Write,
    cursor_shape: CursorShape,
    terminal_options: TerminalOptions,
) -> io::Result<()> {
    if !terminal_options.mouse {
        crossterm::execute!(writer, DisableMouseCapture)?;
    }
    crossterm::execute!(
        writer,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableFocusChange,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        cursor_shape.crossterm_style()
    )
}

fn initialize_terminal_features(
    writer: &mut impl Write,
    cursor_shape: CursorShape,
    terminal_options: TerminalOptions,
) -> io::Result<()> {
    if let Err(error) = write_external_resume_features(writer, cursor_shape, terminal_options) {
        // A failed write may have reached the terminal after changing a
        // feature. While Tau still owns this terminal, best-effort cleanup
        // restores the external-program-safe terminal state.
        let _ = write_external_pause_features(writer, terminal_options);
        return Err(error);
    }
    Ok(())
}

impl Drop for Term {
    fn drop(&mut self) {
        self.shutdown();
        if self.should_write_drop_terminal_cleanup() {
            // Pair the terminal modes we set in `new`: disable paste/focus,
            // pop the keyboard-protocol push, and return cursor shape to the
            // user's configured default so shells and other programs don't
            // inherit Tau's prompt cursor.
            let _ = write_drop_terminal_cleanup(&mut io::stdout(), self.terminal_options);
            let _ = terminal::disable_raw_mode();
        }
    }
}

impl Term {
    fn should_write_drop_terminal_cleanup(&self) -> bool {
        self.owns_raw_mode && !self.handle.lock().terminal.external_paused
    }
}

fn write_drop_terminal_cleanup(
    writer: &mut impl Write,
    terminal_options: TerminalOptions,
) -> io::Result<()> {
    write_external_pause_features(writer, terminal_options)
}

// --- Rendering helpers ---

#[derive(Clone, Debug, PartialEq, Eq)]
enum LineSource {
    Block {
        id: BlockId,
        debug_id: String,
        wrapped_row: usize,
    },
    Input {
        wrapped_row: usize,
    },
    InputScrollIndicator,
}

/// Lays out blocks referenced by an id list, skipping missing ids
/// and blocks with empty content (so callers can "hide" a block by
/// swapping its content to empty without leaving a blank row).
fn layout_id_list(
    ids: &[BlockId],
    blocks: &HashMap<BlockId, StyledBlock>,
    block_debug_ids: &HashMap<BlockId, String>,
    width: usize,
    out: &mut Vec<Vec<Cell>>,
    sources: &mut Vec<LineSource>,
) {
    for id in ids {
        if let Some(block) = blocks.get(id) {
            if block.is_empty() {
                continue;
            }
            let lines = layout_block(block, width);
            for (wrapped_row, line) in lines.into_iter().enumerate() {
                sources.push(LineSource::Block {
                    id: *id,
                    debug_id: block_debug_ids
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| "<unknown>".to_owned()),
                    wrapped_row,
                });
                out.push(line);
            }
        }
    }
}

/// Cached layout for persistent history blocks.
struct HistoryLayoutCache {
    /// Terminal width used to lay out cached entries.
    width: usize,
    /// Shared-state history generation represented by this cache.
    generation: TerminalHistoryGeneration,
    /// Generation represented before the most recent refresh.
    previous_generation: TerminalHistoryGeneration,
    /// Rendered line where an append-only refresh began.
    appended_from_line: Option<usize>,
    /// Start line for each cached history entry plus one final end offset.
    entry_line_offsets: Vec<usize>,
    /// Rendered persistent-history lines.
    lines: Vec<Vec<Cell>>,
    /// Source metadata parallel to `lines`.
    sources: Vec<LineSource>,
}

impl Default for HistoryLayoutCache {
    fn default() -> Self {
        Self {
            width: 0,
            generation: TerminalHistoryGeneration::default(),
            previous_generation: TerminalHistoryGeneration::default(),
            appended_from_line: None,
            entry_line_offsets: vec![0],
            lines: Vec::new(),
            sources: Vec::new(),
        }
    }
}

impl HistoryLayoutCache {
    /// Refreshes the changed history suffix and returns entries laid out.
    fn refresh(&mut self, st: &mut SharedState) -> usize {
        if self.width == st.terminal.width && self.generation == st.layout.history_generation {
            return 0;
        }

        let previous_generation = self.generation;
        let previous_entry_count = self.entry_line_offsets.len().saturating_sub(1);
        let width_changed = self.width != st.terminal.width;
        let requested_dirty_from = st.layout.history_dirty_from.take().unwrap_or(0);
        let can_reuse_prefix = !width_changed
            && requested_dirty_from <= previous_entry_count
            && requested_dirty_from <= st.layout.history.len();
        let dirty_from = if can_reuse_prefix {
            requested_dirty_from
        } else {
            0
        };
        let line_start = self
            .entry_line_offsets
            .get(dirty_from)
            .copied()
            .unwrap_or(0);
        let append_only = can_reuse_prefix
            && dirty_from == previous_entry_count
            && previous_entry_count <= st.layout.history.len();

        self.lines.truncate(line_start);
        self.sources.truncate(line_start);
        self.entry_line_offsets.truncate(dirty_from + 1);
        for id in &st.layout.history[dirty_from..] {
            layout_id_list(
                std::slice::from_ref(id),
                &st.layout.blocks,
                &st.layout.block_debug_ids,
                st.terminal.width,
                &mut self.lines,
                &mut self.sources,
            );
            self.entry_line_offsets.push(self.lines.len());
        }

        self.width = st.terminal.width;
        self.previous_generation = previous_generation;
        self.generation = st.layout.history_generation;
        self.appended_from_line = append_only.then_some(line_start);
        st.layout.history.len().saturating_sub(dirty_from)
    }

    /// Rebuilds an independent cache without consuming redraw dirty state.
    fn rebuild(st: &SharedState) -> Self {
        let mut cache = Self {
            width: st.terminal.width,
            generation: st.layout.history_generation,
            ..Self::default()
        };
        for id in &st.layout.history {
            layout_id_list(
                std::slice::from_ref(id),
                &st.layout.blocks,
                &st.layout.block_debug_ids,
                st.terminal.width,
                &mut cache.lines,
                &mut cache.sources,
            );
            cache.entry_line_offsets.push(cache.lines.len());
        }
        cache
    }
}

/// Layout for everything after persistent history.
struct TailLayout {
    /// Lines for above-active plus fixed prompt/status/suggestions rows.
    lines: Vec<Vec<Cell>>,
    /// Source block/zone for each tail line.
    sources: Vec<LineSource>,
    /// Number of leading `lines` entries that belong to above-active.
    active_height: usize,
    /// Absolute cursor row after persistent history is prepended.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

impl TailLayout {
    fn fixed_height(&self) -> usize {
        self.lines.len().saturating_sub(self.active_height)
    }
}

/// Result of laying out all content.
struct LayoutAll {
    /// All rendered lines without rubber (log + fixed area).
    all_lines: Vec<Vec<Cell>>,
    /// Source block/zone for each rendered line.
    line_sources: Vec<LineSource>,
    /// Index in `all_lines` where the fixed area starts.
    ///
    /// Lines before this are scrollable log content. Lines from this point on
    /// are the prompt/status/suggestions area. Rubber rows may be inserted at
    /// this boundary to absorb visible log shrinkage without moving the fixed
    /// area upward.
    log_end: usize,
    /// Persistent-history generation used to build this layout.
    history_generation: TerminalHistoryGeneration,
    /// Terminal width used to build persistent-history lines.
    history_width: usize,
    /// Number of leading log rows owned by persistent history.
    history_height: usize,
    /// Absolute cursor row in `all_lines`.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

struct ViewPlan {
    /// Top row of the physical terminal viewport within `render_lines`.
    viewport_start: usize,
    rubber_height: usize,
    render_lines: Vec<Vec<Cell>>,
    cursor_row: usize,
}

impl ViewPlan {
    fn visible_start(&self, _height: usize) -> usize {
        self.viewport_start.min(self.render_lines.len())
    }

    fn visible_lines(&self, height: usize) -> &[Vec<Cell>] {
        let start = self.visible_start(height);
        let end = (start + height).min(self.render_lines.len());
        &self.render_lines[start..end]
    }

    fn cursor_in_visible(&self, height: usize) -> usize {
        self.cursor_row.saturating_sub(self.visible_start(height))
    }
}

struct PlanMetrics {
    viewport_start: usize,
    rubber_height: usize,
    render_len: usize,
    cursor_row: usize,
}

/// Renderer-side model of the terminal content Tau believes it owns.
///
/// `viewport_start` is the top row of the physical terminal viewport within
/// the most recent planned `render_lines`. Rows before
/// `viewport_start.min(known_lines.len())` are scrollable log rows already in
/// terminal scrollback. `rubber_height` is temporary blank space inserted
/// between log and fixed rows to absorb visible shrinkage before pulling rows
/// back from scrollback.
#[derive(Default)]
struct TerminalModel {
    /// First absolute row currently represented by the physical viewport.
    viewport_start: usize,
    /// Temporary blank rows retaining the viewport after visible shrinkage.
    rubber_height: usize,
    /// Persistent-history generation represented by `known_lines`.
    history_generation: TerminalHistoryGeneration,
    /// Width used for the represented persistent-history layout.
    history_width: usize,
    /// Leading persistent-history rows in `known_lines`.
    history_height: usize,
    /// Mutable active rows following persistent history in `known_lines`.
    active_height: usize,
    /// Complete represented log rows, including hidden scrollback.
    known_lines: Vec<Vec<Cell>>,
    /// Source metadata parallel to `known_lines`.
    known_sources: Vec<LineSource>,
}

impl TerminalModel {
    fn desired_viewport_start(layout: &LayoutAll, height: usize) -> usize {
        layout.all_lines.len().saturating_sub(height)
    }

    fn history_cache_matches(&self, history: &HistoryLayoutCache) -> bool {
        self.history_generation == history.generation
            && self.history_width == history.width
            && history.lines.len() <= self.known_lines.len()
            && history.sources.len() <= self.known_sources.len()
    }

    fn history_append_matches(&self, history: &HistoryLayoutCache) -> bool {
        self.history_generation == history.previous_generation
            && self.history_width == history.width
            && history.appended_from_line == Some(self.history_height)
            // History is inserted before the mutable active zone. If active rows
            // existed in the prior frame, an append can replace rather than
            // merely follow those rows; use the full hidden-prefix check.
            && self.active_height == 0
    }

    fn hidden_prefix_changed(&self, layout: &LayoutAll) -> bool {
        hidden_lines_changed(
            &self.known_lines,
            &layout.all_lines[..layout.log_end],
            self.viewport_start.min(layout.log_end),
        )
    }

    fn changed_hidden_line(&self, layout: &LayoutAll) -> Option<usize> {
        changed_line_in_range(
            &self.known_lines,
            &layout.all_lines[..layout.log_end],
            0..self.viewport_start.min(layout.log_end),
        )
    }

    fn build_plan(layout: &LayoutAll, viewport_start: usize, rubber_height: usize) -> ViewPlan {
        let mut render_lines = Vec::with_capacity(layout.all_lines.len() + rubber_height);
        render_lines.extend_from_slice(&layout.all_lines[..layout.log_end]);
        render_lines.extend(std::iter::repeat_with(Vec::new).take(rubber_height));
        render_lines.extend_from_slice(&layout.all_lines[layout.log_end..]);

        let cursor_row = if layout.log_end <= layout.cursor_row {
            layout.cursor_row + rubber_height
        } else {
            layout.cursor_row
        };

        ViewPlan {
            viewport_start,
            rubber_height,
            render_lines,
            cursor_row,
        }
    }

    fn full_redraw_plan(layout: &LayoutAll, height: usize) -> ViewPlan {
        let plan = Self::build_plan(layout, Self::desired_viewport_start(layout, height), 0);
        Self::keep_cursor_visible(plan, height)
    }

    #[cfg(test)]
    fn bottom_aligned_plan(layout: &LayoutAll, height: usize) -> ViewPlan {
        let mut plan = Self::build_plan(layout, Self::desired_viewport_start(layout, height), 0);
        plan.viewport_start = plan.visible_start(height);
        plan
    }

    fn keep_cursor_visible(mut plan: ViewPlan, height: usize) -> ViewPlan {
        let height = height.max(1);
        let bottom_start = plan.visible_start(height);
        let viewport_start = viewport_start_with_cursor(
            bottom_start,
            plan.cursor_row,
            plan.render_lines.len(),
            height,
        );

        if viewport_start < bottom_start {
            let viewport_end = (viewport_start + height).min(plan.render_lines.len());
            plan.render_lines.truncate(viewport_end);
        }

        plan.viewport_start = plan.visible_start(height);
        plan
    }

    fn plan_metrics(
        &self,
        log_height: usize,
        fixed_height: usize,
        cursor_row: usize,
        height: usize,
    ) -> PlanMetrics {
        let height = height.max(1);
        let viewport_start = self.viewport_start.min(log_height);
        let mut rubber_height = self.rubber_height;

        if fixed_height < height {
            let occupied = log_height.saturating_sub(viewport_start) + rubber_height + fixed_height;
            if occupied < height {
                // Only create rubber after the viewport has overflowed once.
                // Before that, keep the normal terminal behavior where the
                // prompt follows the transcript instead of being bottom-pinned.
                if 0 < self.viewport_start || 0 < rubber_height {
                    rubber_height += height - occupied;
                }
            } else if height < occupied {
                let overflow = occupied - height;
                let consume_rubber = rubber_height.min(overflow);
                rubber_height -= consume_rubber;
            }
        } else {
            rubber_height = 0;
        }

        let render_len = log_height + rubber_height + fixed_height;
        let cursor_row = if log_height <= cursor_row {
            cursor_row + rubber_height
        } else {
            cursor_row
        };
        let bottom_start = render_len.saturating_sub(height);
        let visible_start =
            viewport_start_with_cursor(bottom_start, cursor_row, render_len, height);
        let render_len = if visible_start < bottom_start {
            (visible_start + height).min(render_len)
        } else {
            render_len
        };

        PlanMetrics {
            viewport_start: render_len.saturating_sub(height),
            rubber_height,
            render_len,
            cursor_row,
        }
    }

    fn plan_view(&self, layout: &LayoutAll, height: usize) -> ViewPlan {
        let fixed_height = layout.all_lines.len().saturating_sub(layout.log_end);
        let metrics = self.plan_metrics(layout.log_end, fixed_height, layout.cursor_row, height);
        let mut plan = Self::build_plan(layout, metrics.viewport_start, metrics.rubber_height);
        plan.cursor_row = metrics.cursor_row;
        plan.render_lines.truncate(metrics.render_len);
        plan.viewport_start = metrics.viewport_start;
        plan
    }

    fn apply_fast_plan(
        &mut self,
        history: &HistoryLayoutCache,
        tail: &TailLayout,
        metrics: &PlanMetrics,
    ) {
        self.viewport_start = metrics.viewport_start;
        self.rubber_height = metrics.rubber_height;
        self.history_generation = history.generation;
        self.history_width = history.width;
        self.known_lines.truncate(self.history_height);
        self.known_sources.truncate(self.history_height);
        self.known_lines
            .extend_from_slice(&history.lines[self.history_height..]);
        self.known_sources
            .extend_from_slice(&history.sources[self.history_height..]);
        self.history_height = history.lines.len();
        self.active_height = tail.active_height;
        self.known_lines
            .extend_from_slice(&tail.lines[..tail.active_height]);
        self.known_sources
            .extend_from_slice(&tail.sources[..tail.active_height]);
    }

    fn reset_to_layout(&mut self, layout: &LayoutAll, viewport_start: usize, rubber_height: usize) {
        self.viewport_start = viewport_start;
        self.rubber_height = rubber_height;
        self.history_generation = layout.history_generation;
        self.history_width = layout.history_width;
        self.history_height = layout.history_height;
        self.active_height = layout.log_end.saturating_sub(layout.history_height);
        self.known_lines = layout.all_lines[..layout.log_end].to_vec();
        self.known_sources = layout.line_sources[..layout.log_end].to_vec();
    }
}

fn prompt_input_max_rows(terminal_height: usize) -> usize {
    (terminal_height.max(1) * PROMPT_INPUT_MAX_HEIGHT_PERCENT / 100).max(1)
}

fn prompt_scroll_indicator_rows(
    show_indicator: bool,
    buffer_non_empty: bool,
    total_rows: usize,
    cap_rows: usize,
) -> usize {
    usize::from(show_indicator && buffer_non_empty && 2 <= cap_rows && cap_rows < total_rows)
}

fn prompt_editable_rows(total_rows: usize, cap_rows: usize, indicator_rows: usize) -> usize {
    cap_rows
        .saturating_sub(indicator_rows)
        .max(1)
        .min(total_rows.max(1))
}

fn prompt_scroll_indicator_text(
    start: usize,
    visible_rows: usize,
    total_rows: usize,
    width: usize,
) -> String {
    let end = (start + visible_rows).min(total_rows);
    let hidden_above = start;
    let hidden_below = total_rows.saturating_sub(end);
    let full = format!(
        "↕ prompt rows {}-{}/{}  ↑{} ↓{}",
        start + 1,
        end,
        total_rows,
        hidden_above,
        hidden_below
    );
    if display_width(&full) <= width {
        return full;
    }
    let compact = format!("↕ ↑{} ↓{}", hidden_above, hidden_below);
    if display_width(&compact) <= width {
        return compact;
    }
    truncate_to_width("↕", width)
}

fn layout_tail(st: &SharedState, history_height: usize) -> TailLayout {
    let width = st.terminal.width;
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut sources: Vec<LineSource> = Vec::new();

    layout_id_list(
        &st.layout.above_active,
        &st.layout.blocks,
        &st.layout.block_debug_ids,
        width,
        &mut lines,
        &mut sources,
    );
    let active_height = lines.len();
    layout_id_list(
        &st.layout.above_sticky,
        &st.layout.blocks,
        &st.layout.block_debug_ids,
        width,
        &mut lines,
        &mut sources,
    );

    let above_end = history_height + lines.len();

    let mut input_content = st.editor.left_prompt.clone();
    if st.editor.buffer.is_empty() {
        for span in st.editor.input_placeholder.spans() {
            input_content.push(span.clone());
        }
    } else {
        input_content.push(Span::plain(&st.editor.buffer));
    }
    // Preserve a trailing-newline blank row so a buffer ending in
    // `\n` (the user just hit Shift+Enter / Alt+Enter) gives the
    // cursor somewhere to sit and the prompt grows immediately
    // rather than only after the next typed character.
    let mut input_lines = layout_lines()
        .content(&input_content)
        .width(width)
        .preserve_last_newline(true)
        .call();

    let left_cols = st.editor.left_prompt.char_count();
    let (buffer_cursor_row, cursor_col) =
        buffer_position_for_byte(&st.editor.buffer, st.editor.cursor, width, left_cols);
    // Prompt input is special because it owns a visible cursor. When the
    // cursor sits at the end and the final column has just been filled, it
    // must appear immediately at column 0 of the next visual row, growing the
    // prompt height before any further character is typed. This is easy to
    // overlook and has regressed repeatedly. Do not move this behavior into
    // general block layout: static blocks have no cursor and must not gain a
    // phantom trailing row just because their content exactly fills a line.
    while input_lines.len() <= buffer_cursor_row {
        input_lines.push(Vec::new());
    }

    if !st.editor.right_prompt.is_empty() && !input_lines.is_empty() {
        let first_line = &input_lines[0];
        let right_cells = st.editor.right_prompt.to_cells();
        let first_cols: usize = first_line.iter().map(|c| c.col_width()).sum();
        let right_cols: usize = right_cells.iter().map(|c| c.col_width()).sum();
        let needed = first_cols + 1 + right_cols;
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_cols - right_cols;
            let mut padded = first_line.clone();
            padded.extend(std::iter::repeat_n(Cell::plain(' '), padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    let input_total_rows = input_lines.len().max(1);
    let cap_rows = prompt_input_max_rows(st.terminal.height);
    let indicator_rows = prompt_scroll_indicator_rows(
        st.editor.show_prompt_scroll_indicator,
        !st.editor.buffer.is_empty(),
        input_total_rows,
        cap_rows,
    );
    let visible_input_rows = prompt_editable_rows(input_total_rows, cap_rows, indicator_rows);
    let viewport_start = viewport_start_with_cursor(
        st.editor.input_viewport_start,
        buffer_cursor_row,
        input_total_rows,
        visible_input_rows,
    );
    let cursor_row = above_end + indicator_rows + buffer_cursor_row.saturating_sub(viewport_start);

    if indicator_rows == 1 {
        let indicator = prompt_scroll_indicator_text(
            viewport_start,
            visible_input_rows,
            input_total_rows,
            width,
        );
        sources.push(LineSource::InputScrollIndicator);
        lines.push(StyledText::from(indicator).to_cells());
    }

    let viewport_end = (viewport_start + visible_input_rows).min(input_lines.len());
    for (wrapped_row, line) in input_lines
        .into_iter()
        .enumerate()
        .skip(viewport_start)
        .take(viewport_end.saturating_sub(viewport_start))
    {
        sources.push(LineSource::Input { wrapped_row });
        lines.push(line);
    }
    layout_id_list(
        &st.layout.suggestions,
        &st.layout.blocks,
        &st.layout.block_debug_ids,
        width,
        &mut lines,
        &mut sources,
    );
    layout_id_list(
        &st.layout.below,
        &st.layout.blocks,
        &st.layout.block_debug_ids,
        width,
        &mut lines,
        &mut sources,
    );

    TailLayout {
        lines,
        sources,
        active_height,
        cursor_row,
        cursor_col,
    }
}

fn layout_all_from_cached_history(history: &HistoryLayoutCache, tail: TailLayout) -> LayoutAll {
    let log_end = history.lines.len() + tail.active_height;
    let cursor_row = tail.cursor_row;
    let cursor_col = tail.cursor_col;
    let mut all_lines = Vec::with_capacity(history.lines.len() + tail.lines.len());
    all_lines.extend_from_slice(&history.lines);
    all_lines.extend(tail.lines);

    let mut line_sources = Vec::with_capacity(history.sources.len() + tail.sources.len());
    line_sources.extend_from_slice(&history.sources);
    line_sources.extend(tail.sources);

    LayoutAll {
        all_lines,
        line_sources,
        log_end,
        history_generation: history.generation,
        history_width: history.width,
        history_height: history.lines.len(),
        cursor_row,
        cursor_col,
    }
}

/// Lays out the full content (history + above + input + below).
fn layout_all(st: &SharedState) -> LayoutAll {
    let history = HistoryLayoutCache::rebuild(st);
    let tail = layout_tail(st, history.lines.len());
    layout_all_from_cached_history(&history, tail)
}

fn visible_lines_from_parts(
    history_lines: &[Vec<Cell>],
    tail: &TailLayout,
    metrics: &PlanMetrics,
) -> Vec<Vec<Cell>> {
    render_rows_from(history_lines, tail, metrics, metrics.viewport_start)
}

/// Materializes rendered rows from `start` through the current plan end.
fn render_rows_from(
    history_lines: &[Vec<Cell>],
    tail: &TailLayout,
    metrics: &PlanMetrics,
    start: usize,
) -> Vec<Vec<Cell>> {
    let history_height = history_lines.len();
    let log_height = history_height + tail.active_height;
    let fixed_start = log_height + metrics.rubber_height;
    let mut rows = Vec::with_capacity(metrics.render_len.saturating_sub(start));

    for idx in start..metrics.render_len {
        if idx < history_height {
            rows.push(
                history_lines
                    .get(idx)
                    .expect("requested history row should exist")
                    .clone(),
            );
        } else if idx < log_height {
            rows.push(
                tail.lines
                    .get(idx - history_height)
                    .expect("requested active row should exist")
                    .clone(),
            );
        } else if idx < fixed_start {
            rows.push(Vec::new());
        } else {
            rows.push(
                tail.lines
                    .get(tail.active_height + idx - fixed_start)
                    .expect("requested fixed row should exist")
                    .clone(),
            );
        }
    }

    rows
}

/// Builds the bounded scrolling input rebased at the prior viewport.
fn scrolling_suffix(
    history_lines: &[Vec<Cell>],
    tail: &TailLayout,
    metrics: &PlanMetrics,
    terminal_model: &TerminalModel,
) -> Vec<Vec<Cell>> {
    render_rows_from(history_lines, tail, metrics, terminal_model.viewport_start)
}

// --- Redraw thread ---

enum RenderFrame {
    Fast {
        tail: TailLayout,
        metrics: PlanMetrics,
    },
    Full {
        layout: LayoutAll,
    },
}

struct RedrawPass {
    width: usize,
    height: usize,
    size_changed: bool,
    force_full: bool,
    sync_gen: RedrawSyncGeneration,
    pending_raw: Vec<String>,
    redraw_history_size: usize,
    frame: RenderFrame,
    /// Bounded opaque observations captured with this frame's layout.
    presentation_observations: Option<CapturedPresentationObservations>,
}

struct FullRenderMark {
    reason: &'static str,
    prev_visible_start: usize,
    visible_start: usize,
    height: usize,
    changed_line: Option<usize>,
    previous_source: Option<LineSource>,
}

struct FullRenderMarkInput {
    reason: &'static str,
    changed_line: Option<usize>,
    previous_source: Option<LineSource>,
}

fn redraw_loop(
    state: Arc<Mutex<SharedState>>,
    notify_rx: tau_blocking_notify_channel::Receiver,
    writer: Box<dyn Write + Send>,
    input_tx: path_std_sync::mpsc::Sender<InputMessage>,
    sync_condvar: &std::sync::Condvar,
) {
    let mut writer = BufWriter::new(writer);
    let (w, h) = {
        let st = state.lock().expect("term state mutex poisoned");
        (st.terminal.width, st.terminal.height)
    };
    let mut screen = Screen::new(w);
    let mut prev_width = w;
    let mut prev_height = h;
    let mut history_cache = HistoryLayoutCache::default();
    let mut terminal_model = TerminalModel::default();

    loop {
        if render_shutdown_if_requested(
            &state,
            &mut writer,
            &mut screen,
            &terminal_model,
            prev_width,
            sync_condvar,
        ) {
            break;
        }

        if !wait_for_redraw_or_sync(&state, &notify_rx) {
            break;
        }

        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            "redraw prepare started"
        );
        let pass = match prepare_redraw_pass(
            &state,
            &mut history_cache,
            &terminal_model,
            prev_width,
            prev_height,
            sync_condvar,
        ) {
            Some(pass) => pass,
            None => continue,
        };
        let write_started = path_std_time::Instant::now();
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            "terminal write started"
        );
        let render_result = render_redraw_pass(
            &state,
            &mut writer,
            &mut screen,
            &history_cache,
            &mut terminal_model,
            &pass,
        );
        let write_elapsed = write_started.elapsed();
        if let Err(error) = render_result {
            trace_failed_presentation_observations(&state, &pass, "write", write_elapsed, &error);
            fail_terminal_output(&state, &input_tx, sync_condvar, error);
            discard_failed_output(writer);
            return;
        }
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            write_us = write_elapsed.as_micros(),
            "terminal write finished; flush started"
        );
        let flush_started = path_std_time::Instant::now();
        let output_result = writer.flush();
        let flush_elapsed = flush_started.elapsed();
        if let Err(error) = output_result {
            trace_failed_presentation_observations(&state, &pass, "flush", flush_elapsed, &error);
            fail_terminal_output(&state, &input_tx, sync_condvar, error);
            discard_failed_output(writer);
            return;
        }
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            write_us = write_elapsed.as_micros(),
            flush_us = flush_elapsed.as_micros(),
            "terminal write and flush finished"
        );
        trace_flushed_presentation_observations(&state, &pass);
        if (Duration::from_millis(500) <= write_elapsed
            || Duration::from_millis(500) <= flush_elapsed)
            && admit_stall_warning()
        {
            tracing::warn!(
                target: "tau_cli_term_raw::frontend_progress",
                write_ms = write_elapsed.as_millis(),
                flush_ms = flush_elapsed.as_millis(),
                "terminal output stalled"
            );
        }

        prev_width = pass.width;
        prev_height = pass.height;

        complete_redraw_sync(&state, pass.sync_gen, sync_condvar);
    }
}

/// Drops a failed buffered writer without retrying bytes retained in its
/// userspace buffer.
fn discard_failed_output(writer: BufWriter<Box<dyn Write + Send>>) {
    let _ = writer.into_parts();
}

fn render_shutdown_if_requested(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &TerminalModel,
    prev_width: usize,
    sync_condvar: &std::sync::Condvar,
) -> bool {
    let mut st = state.lock().expect("term state mutex poisoned");
    if !st.terminal.shutdown {
        return false;
    }
    if st.terminal.external_paused {
        st.terminal.sync_completed = st.terminal.sync_requested;
        drop(st);
        sync_condvar.notify_all();
        return true;
    }

    // Final render + move cursor below all content.
    let layout = layout_all(&st);
    let height = st.terminal.height.max(1);
    let plan = terminal_model.plan_view(&layout, height);
    let visible = plan.visible_lines(height);
    let cursor_in_visible = plan.cursor_in_visible(height);
    drop(st);

    screen.set_width(prev_width);
    let _ = screen.update(writer, visible, (cursor_in_visible, layout.cursor_col));
    let below = plan.render_lines.len().saturating_sub(plan.cursor_row + 1);
    for _ in 0..=below {
        let _ = writer.queue(crossterm::style::Print("\r\n"));
    }
    let _ = writer.flush();
    {
        let mut st = state.lock().expect("term state mutex poisoned");
        st.terminal.sync_completed = st.terminal.sync_requested;
    }
    sync_condvar.notify_all();
    true
}

fn wait_for_redraw_or_sync(
    state: &Arc<Mutex<SharedState>>,
    notify_rx: &tau_blocking_notify_channel::Receiver,
) -> bool {
    // If a sync was requested but not yet completed, skip blocking on recv and
    // render immediately. Otherwise block until the next notification arrives.
    let trace_enabled = tracing::enabled!(
        target: "tau_cli_term_raw::frontend_progress",
        tracing::Level::TRACE
    );
    let lock_started = trace_enabled.then(path_std_time::Instant::now);
    let st = state.lock().expect("term state mutex poisoned");
    if let Some(lock_started) = lock_started {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            lock_wait_us = lock_started.elapsed().as_micros(),
            stage = "notification_check",
            "terminal shared state acquired"
        );
    }
    if st.terminal.sync_completed < st.terminal.sync_requested {
        return true;
    }
    drop(st);
    let notification_started = trace_enabled.then(path_std_time::Instant::now);
    let result = notify_rx.recv().is_ok();
    if let Some(notification_started) = notification_started {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            notification_wait_us = notification_started.elapsed().as_micros(),
            "redraw notification wait finished"
        );
    }
    result
}

fn prepare_redraw_pass(
    state: &Arc<Mutex<SharedState>>,
    history_cache: &mut HistoryLayoutCache,
    terminal_model: &TerminalModel,
    prev_width: usize,
    prev_height: usize,
    sync_condvar: &std::sync::Condvar,
) -> Option<RedrawPass> {
    let trace_enabled = tracing::enabled!(
        target: "tau_cli_term_raw::frontend_progress",
        tracing::Level::TRACE
    );
    let lock_started = trace_enabled.then(path_std_time::Instant::now);
    let mut st = state.lock().expect("term state mutex poisoned");
    if let Some(lock_started) = lock_started {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            lock_wait_us = lock_started.elapsed().as_micros(),
            stage = "redraw_prepare",
            "terminal shared state acquired"
        );
    }
    if st.terminal.redraw_suppression != 0 {
        // The notification that woke this pass has been consumed. Preserve it
        // for the outermost suppression guard so a no-op transaction cannot
        // swallow another producer's already-pending redraw.
        st.terminal.redraw_dirty_while_suppressed = true;
        st.terminal.sync_completed = st.terminal.sync_requested;
        sync_condvar.notify_all();
        return None;
    }
    if st.terminal.external_paused {
        st.terminal.sync_completed = st.terminal.sync_requested;
        sync_condvar.notify_all();
        return None;
    }
    let width = st.terminal.width;
    let height = st.terminal.height.max(1);
    let size_changed = prev_width != width || prev_height != height;
    // Take-and-clear so the flag is one-shot.
    let force_full = std::mem::take(&mut st.terminal.invalidate_screen);
    // Capture the sync generation we're rendering against. We must not advance
    // sync_completed beyond this value, because a later bump to sync_requested
    // may have arrived with state changes we haven't read yet.
    let sync_gen = st.terminal.sync_requested;
    let pending_raw = std::mem::take(&mut st.terminal.pending_raw);
    let redraw_history_size = st.terminal.redraw_history_size;
    let presentation_observations =
        (!st.presentation_observations.is_empty()).then(|| st.presentation_observations.capture());

    let preparation_started = trace_enabled.then(path_std_time::Instant::now);
    history_cache.refresh(&mut st);
    let tail = layout_tail(&st, history_cache.lines.len());
    let log_height = history_cache.lines.len() + tail.active_height;
    let fixed_height = tail.fixed_height();
    let metrics = terminal_model.plan_metrics(log_height, fixed_height, tail.cursor_row, height);
    let can_fast = !size_changed
        && !force_full
        && ((terminal_model.history_cache_matches(history_cache)
            && metrics.viewport_start == terminal_model.viewport_start)
            || (terminal_model.history_append_matches(history_cache)
                && terminal_model.viewport_start <= metrics.viewport_start))
        && metrics.viewport_start <= history_cache.lines.len();
    let frame = if can_fast {
        RenderFrame::Fast { tail, metrics }
    } else {
        RenderFrame::Full {
            layout: layout_all_from_cached_history(history_cache, tail),
        }
    };
    let pass = RedrawPass {
        width,
        height,
        size_changed,
        force_full,
        sync_gen,
        pending_raw,
        redraw_history_size,
        frame,
        presentation_observations,
    };
    if let Some(preparation_started) = preparation_started {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            preparation_us = preparation_started.elapsed().as_micros(),
            "redraw layout prepared"
        );
    }
    Some(pass)
}

/// Reports only successful frame correlation after every pass write and flush.
fn trace_flushed_presentation_observations(_state: &Arc<Mutex<SharedState>>, pass: &RedrawPass) {
    let Some(observations) = &pass.presentation_observations else {
        return;
    };
    let flushed_at = path_std_time::Instant::now();
    for fact in &observations.facts {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            delivery_id = fact.delivery_id.get(),
            fact = fact.fact,
            mutation_generation = fact.generation.get(),
            frame_generation = observations.generation.get(),
            mutation_to_flush_us = flushed_at.duration_since(fact.observed_at).as_micros(),
            "selected presentation mutation frame written and flushed"
        );
    }
    if observations.omitted != 0 {
        tracing::trace!(
            target: "tau_cli_term_raw::frontend_progress",
            frame_generation = observations.generation.get(),
            omitted = observations.omitted,
            "selected presentation flush observations omitted"
        );
    }
    #[cfg(test)]
    _state
        .lock()
        .expect("term state mutex poisoned")
        .presentation_observations
        .record_success_for_test(observations);
}

/// Reports bounded failed-pass context without making a successful-frame claim.
fn trace_failed_presentation_observations(
    _state: &Arc<Mutex<SharedState>>,
    pass: &RedrawPass,
    stage: &'static str,
    stage_elapsed: Duration,
    error: &io::Error,
) {
    let Some(observations) = &pass.presentation_observations else {
        return;
    };
    #[cfg(test)]
    _state
        .lock()
        .expect("term state mutex poisoned")
        .presentation_failure_test_records
        .push((
            stage,
            stage_elapsed.as_micros(),
            observations.facts.len(),
            observations.omitted,
        ));
    tracing::trace!(
        target: "tau_cli_term_raw::frontend_progress",
        stage,
        stage_us = stage_elapsed.as_micros(),
        frame_generation = observations.generation.get(),
        indeterminate_facts = observations.facts.len(),
        omitted = observations.omitted,
        error_kind = ?error.kind(),
        "selected presentation redraw pass failed or is indeterminate"
    );
}

fn render_redraw_pass(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    history_cache: &HistoryLayoutCache,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
) -> io::Result<()> {
    // Pending escape sequences: emit before the frame so they sit outside any
    // synchronized-update bracket the renderer installs. SetUserVar and similar
    // OSC sequences don't affect visible state, so ordering relative to the
    // frame doesn't matter for correctness — putting them first just avoids any
    // chance of interleaving with a deferred frame.
    for seq in &pass.pending_raw {
        writer.write_all(seq.as_bytes())?;
    }
    if pass.force_full {
        // The terminal was clobbered by an external program ($EDITOR returned).
        // Wipe Screen's cached idea of what's on the terminal so `full_render`
        // redraws from scratch.
        screen.invalidate();
    }

    match &pass.frame {
        RenderFrame::Fast { tail, metrics } => {
            render_fast_frame(
                writer,
                screen,
                history_cache,
                terminal_model,
                pass,
                tail,
                metrics,
            )?;
        }
        RenderFrame::Full { layout } => {
            render_full_frame(state, writer, screen, terminal_model, pass, layout)?;
        }
    }
    Ok(())
}

fn render_fast_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    history_cache: &HistoryLayoutCache,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    tail: &TailLayout,
    metrics: &PlanMetrics,
) -> io::Result<()> {
    screen.set_width(pass.width);
    if terminal_model.viewport_start < metrics.viewport_start {
        let previous_viewport_start = terminal_model.viewport_start;
        // `Screen` retains only the old visible rows. Rebase the bounded suffix
        // beginning at that viewport to row zero: `prev_viewport_top = 0` then
        // describes the same physical screen without copying or comparing older
        // terminal scrollback.
        let suffix = scrolling_suffix(&history_cache.lines, tail, metrics, terminal_model);
        let cursor_row = metrics.cursor_row.saturating_sub(previous_viewport_start);
        screen.render_scrolling(
            writer,
            &suffix,
            0,
            pass.height,
            (cursor_row, tail.cursor_col),
        )?;
    } else {
        let visible = visible_lines_from_parts(&history_cache.lines, tail, metrics);
        let cursor_in_visible = metrics.cursor_row.saturating_sub(metrics.viewport_start);
        screen.update(writer, &visible, (cursor_in_visible, tail.cursor_col))?;
    }
    terminal_model.apply_fast_plan(history_cache, tail, metrics);
    Ok(())
}

fn render_full_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    if pass.size_changed || pass.force_full {
        let reason = if pass.size_changed {
            "size_changed"
        } else {
            "force_full"
        };
        render_marked_full_frame(
            state,
            writer,
            screen,
            terminal_model,
            pass,
            layout,
            FullRenderMarkInput {
                reason,
                changed_line: None,
                previous_source: None,
            },
        )?;
        return Ok(());
    }

    render_incremental_or_scroll_frame(state, writer, screen, terminal_model, pass, layout)
}

fn render_incremental_or_scroll_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    screen.set_width(pass.width);

    let hidden_prefix_changed = terminal_model.hidden_prefix_changed(layout);
    let incremental_plan = terminal_model.plan_view(layout, pass.height);
    let incremental_visible_start = incremental_plan.viewport_start;

    if incremental_visible_start < terminal_model.viewport_start {
        render_viewport_moved_up_frame(state, writer, screen, terminal_model, pass, layout)
    } else if hidden_prefix_changed {
        render_hidden_prefix_changed_frame(state, writer, screen, terminal_model, pass, layout)
    } else if terminal_model.viewport_start < incremental_visible_start {
        render_scrolling_frame(
            writer,
            screen,
            terminal_model,
            pass,
            layout,
            incremental_plan,
        )
    } else {
        render_diff_frame(
            writer,
            screen,
            terminal_model,
            pass,
            layout,
            incremental_plan,
        )
    }
}

fn render_marked_full_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    mark_input: FullRenderMarkInput,
) -> io::Result<()> {
    let plan = TerminalModel::full_redraw_plan(layout, pass.height);
    let mark = FullRenderMark {
        reason: mark_input.reason,
        prev_visible_start: terminal_model.viewport_start,
        visible_start: plan.viewport_start,
        height: pass.height,
        changed_line: mark_input.changed_line,
        previous_source: mark_input.previous_source,
    };
    mark_full_render(state, layout, mark);
    full_render(
        writer,
        screen,
        layout,
        &plan,
        pass.width,
        pass.height,
        pass.redraw_history_size,
    )?;
    reset_model_after_rendered_full_frame(terminal_model, pass, layout, plan);
    Ok(())
}

fn render_viewport_moved_up_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    // The desired viewport moved upward to keep the input cursor visible. Rows
    // that should re-enter the screen may currently exist only in terminal
    // scrollback, which cannot be pulled back incrementally. Since we are
    // repainting from scratch, discard any rubber and paint the new viewport
    // directly.
    render_marked_full_frame(
        state,
        writer,
        screen,
        terminal_model,
        pass,
        layout,
        FullRenderMarkInput {
            reason: "viewport_moved_up",
            changed_line: None,
            previous_source: None,
        },
    )
}

fn render_hidden_prefix_changed_frame(
    state: &Arc<Mutex<SharedState>>,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
) -> io::Result<()> {
    // The terminal scrollback may contain rows whose logical content changed.
    // Clear it instead of trying to patch it incrementally. Since we are
    // repainting from scratch, discard any rubber and paint the new viewport
    // directly.
    let changed_line = terminal_model.changed_hidden_line(layout);
    let previous_source = changed_line
        .and_then(|idx| terminal_model.known_sources.get(idx))
        .cloned();
    render_marked_full_frame(
        state,
        writer,
        screen,
        terminal_model,
        pass,
        layout,
        FullRenderMarkInput {
            reason: "hidden_prefix_changed",
            changed_line,
            previous_source,
        },
    )
}

fn render_scrolling_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    plan: ViewPlan,
) -> io::Result<()> {
    // Content pushed log rows off the top. Use the scrolling renderer
    // (Pi-style). Rubber is part of the virtual tail, so it shrinks before any
    // extra log row enters scrollback.
    screen.render_scrolling(
        writer,
        &plan.render_lines,
        terminal_model.viewport_start,
        pass.height,
        (plan.cursor_row, layout.cursor_col),
    )?;
    terminal_model.reset_to_layout(layout, plan.viewport_start, plan.rubber_height);
    Ok(())
}

fn render_diff_frame(
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    screen: &mut Screen,
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    plan: ViewPlan,
) -> io::Result<()> {
    // No new scrollback rows — normal differential update. This includes visible
    // shrinkage: rubber grows instead of moving the viewport upward.
    let visible = plan.visible_lines(pass.height);
    let cursor_in_visible = plan.cursor_in_visible(pass.height);
    screen.update(writer, visible, (cursor_in_visible, layout.cursor_col))?;
    terminal_model.reset_to_layout(layout, plan.viewport_start, plan.rubber_height);
    Ok(())
}

fn reset_model_after_rendered_full_frame(
    terminal_model: &mut TerminalModel,
    pass: &RedrawPass,
    layout: &LayoutAll,
    plan: ViewPlan,
) {
    let viewport_start =
        full_render_effective_viewport_start(layout, &plan, pass.height, pass.redraw_history_size);
    terminal_model.reset_to_layout(layout, viewport_start, plan.rubber_height);
}

fn complete_redraw_sync(
    state: &Arc<Mutex<SharedState>>,
    sync_gen: RedrawSyncGeneration,
    sync_condvar: &std::sync::Condvar,
) {
    // Advance sync_completed to the generation we captured before rendering.
    // Using max() is defensive — renders are sequential so sync_gen is
    // monotonically increasing, but max() makes the invariant explicit.
    {
        let mut st = state.lock().expect("term state mutex poisoned");
        st.terminal.sync_completed = st.terminal.sync_completed.max(sync_gen);
    }
    sync_condvar.notify_all();
}

/// Records the first output error, releases terminal waiters, and wakes the
/// attachment's input owner.
fn fail_terminal_output(
    state: &Arc<Mutex<SharedState>>,
    input_tx: &path_std_sync::mpsc::Sender<InputMessage>,
    sync_condvar: &std::sync::Condvar,
    error: io::Error,
) {
    let mut st = state.lock().expect("term state mutex poisoned");
    if st.terminal.output_failure.is_none() {
        tracing::error!(
            target: "tau_cli_term_raw::redraw",
            error = %error,
            "terminal output failed; stopping attachment renderer"
        );
        st.terminal.output_failure = Some(OutputFailure::new(error));
    }
    st.terminal.input_shutdown = true;
    st.terminal.sync_completed = st.terminal.sync_requested;
    drop(st);
    sync_condvar.notify_all();
    let _ = input_tx.send(InputMessage::Shutdown);
}

fn changed_line_in_range(
    prev_all_lines: &[Vec<Cell>],
    all_lines: &[Vec<Cell>],
    range: std::ops::Range<usize>,
) -> Option<usize> {
    range
        .into_iter()
        .find(|idx| prev_all_lines.get(*idx) != all_lines.get(*idx))
}

fn mark_full_render(state: &Arc<Mutex<SharedState>>, layout: &LayoutAll, mark: FullRenderMark) {
    let full_render_count = {
        let mut st = state.lock().expect("term state mutex poisoned");
        st.terminal.full_render_count += 1;
        st.terminal.full_render_count
    };
    let current_source = mark
        .changed_line
        .and_then(|idx| layout.line_sources.get(idx))
        .cloned();
    let previous = describe_line_source(mark.previous_source.as_ref());
    let current = describe_line_source(current_source.as_ref());
    tracing::info!(
        target: "tau_cli_term_raw::redraw",
        full_render_count,
        reason = mark.reason,
        prev_visible_start = mark.prev_visible_start,
        visible_start = mark.visible_start,
        height = mark.height,
        total_lines = layout.all_lines.len(),
        changed_line = mark.changed_line,
        previous_source = ?mark.previous_source,
        current_source = ?current_source,
        "full redraw caused by {}: {previous} -> {current}", mark.reason
    );
    tracing::trace!(
        target: "tau_cli_term_raw::redraw",
        full_render_count,
        reason = mark.reason,
        prev_visible_start = mark.prev_visible_start,
        visible_start = mark.visible_start,
        height = mark.height,
        total_lines = layout.all_lines.len(),
        changed_line = mark.changed_line,
        previous_source = ?mark.previous_source,
        current_source = ?current_source,
        "full render"
    );
}

fn describe_line_source(source: Option<&LineSource>) -> String {
    match source {
        Some(LineSource::Block {
            id,
            debug_id,
            wrapped_row,
        }) => format!("block {:?} `{}` row {}", id, debug_id, wrapped_row),
        Some(LineSource::Input { wrapped_row }) => format!("input row {wrapped_row}"),
        Some(LineSource::InputScrollIndicator) => "input scroll indicator".to_owned(),
        None => "<missing>".to_owned(),
    }
}

fn viewport_start_with_cursor(
    viewport_start: usize,
    cursor_row: usize,
    total_rows: usize,
    height: usize,
) -> usize {
    let height = height.max(1);
    let max_start = total_rows.saturating_sub(height);
    let mut start = viewport_start.min(max_start);

    if cursor_row < start {
        start = cursor_row;
    } else if start + height <= cursor_row {
        start = (cursor_row + 1).saturating_sub(height);
    }

    start.min(max_start)
}

fn hidden_lines_changed(
    prev_all_lines: &[Vec<Cell>],
    all_lines: &[Vec<Cell>],
    prev_visible_start: usize,
) -> bool {
    (0..prev_visible_start).any(|idx| prev_all_lines.get(idx) != all_lines.get(idx))
}

fn full_render_replay_start(
    layout: &LayoutAll,
    plan: &ViewPlan,
    redraw_history_size: usize,
) -> usize {
    let total = plan.render_lines.len();
    let log_end = layout.log_end.min(total);
    log_end.saturating_sub(redraw_history_size)
}

fn full_render_effective_viewport_start(
    layout: &LayoutAll,
    plan: &ViewPlan,
    height: usize,
    redraw_history_size: usize,
) -> usize {
    let replay_start = full_render_replay_start(layout, plan, redraw_history_size);
    let replay_len = plan.render_lines.len().saturating_sub(replay_start);
    if height < replay_len {
        plan.render_lines.len().saturating_sub(height)
    } else {
        replay_start
    }
}

/// Full re-render: clear screen + scrollback, output the configured suffix of
/// rendered history/log rows plus the fixed tail, and position the cursor. Used
/// on resize and after invalidation. Callers should pass a no-rubber plan so a
/// full repaint drops rubber instead of preserving temporary blank space.
/// Overflow rebuilds recent terminal scrollback naturally. After rendering,
/// Screen tracks the visible viewport for subsequent differential updates.
fn full_render(
    stdout: &mut impl Write,
    screen: &mut Screen,
    layout: &LayoutAll,
    plan: &ViewPlan,
    width: usize,
    height: usize,
    redraw_history_size: usize,
) -> io::Result<()> {
    screen.set_width(width);

    let all_lines = &plan.render_lines;
    let replay_start = full_render_replay_start(layout, plan, redraw_history_size);
    let replay_lines = &all_lines[replay_start..];
    let replay_total = replay_lines.len();
    let effective_viewport_start =
        full_render_effective_viewport_start(layout, plan, height, redraw_history_size);

    with_synchronized_update(stdout, |stdout| {
        // Clear screen, home cursor, and clear scrollback. The scrollback is rebuilt
        // by replaying the capped no-rubber suffix below. Disable autowrap while
        // replaying so exact-width rows don't create phantom blank rows before the
        // explicit CRLF between logical rows.
        stdout.queue(Print("\x1b[2J\x1b[H\x1b[3J\x1b[?7l"))?;

        // Output the capped logical suffix starting at the top. Overflow scrolls
        // into scrollback naturally. Short content stays at the top, so the prompt
        // sits directly under content instead of being bottom-pinned by rubber.
        for (i, line) in replay_lines.iter().enumerate() {
            if 0 < i {
                stdout.queue(Print("\r\n"))?;
            }
            emit_styled_cells(stdout, line)?;
        }

        stdout.queue(Print("\x1b[?7h"))?;

        // After outputting, the cursor is at the last content line. When content
        // overflowed, that line is at the terminal bottom; otherwise it is at its
        // natural row below the transcript.
        let current_screen_row = if height <= replay_total {
            height - 1
        } else {
            replay_total.saturating_sub(1)
        };

        let cursor_screen_row = plan.cursor_row.saturating_sub(effective_viewport_start);

        let up = current_screen_row.saturating_sub(cursor_screen_row);
        if 0 < up {
            stdout.queue(MoveUp(up as u16))?;
        }
        stdout.queue(MoveToColumn(layout.cursor_col as u16))?;
        Ok(())
    })?;

    // Track what's visible on the terminal so the next
    // screen.update() can diff correctly.
    let visible_end = (effective_viewport_start + height).min(plan.render_lines.len());
    let visible_lines = plan.render_lines[effective_viewport_start..visible_end].to_vec();
    let cursor_in_visible = plan.cursor_row.saturating_sub(effective_viewport_start);
    screen.reset_to(visible_lines, cursor_in_visible, layout.cursor_col);

    Ok(())
}

/// Queues one balanced synchronized-update transaction without flushing.
///
/// The closing marker is attempted even when `body` fails. If both operations
/// fail, the body error takes precedence because it identifies the first loss
/// of frame output.
fn with_synchronized_update<W, F>(writer: &mut W, body: F) -> io::Result<()>
where
    W: Write,
    F: FnOnce(&mut W) -> io::Result<()>,
{
    writer.queue(terminal::BeginSynchronizedUpdate)?;
    let body_result = body(writer);
    let end_result = writer.queue(terminal::EndSynchronizedUpdate).map(|_| ());
    body_result.and(end_result)
}

// --- Helpers ---

fn move_cursor_vertical(st: &SharedState, delta: isize, target_col: usize) -> Option<usize> {
    let width = st.terminal.width.max(1);
    let left_cols = st.editor.left_prompt.char_count();
    let (current_row, _) =
        buffer_position_for_byte(&st.editor.buffer, st.editor.cursor, width, left_cols);

    let target_row = current_row as isize + delta;
    if target_row < 0 {
        return None;
    }
    let target_row = target_row as usize;

    let (max_row, _) = buffer_end_position(&st.editor.buffer, width, left_cols);
    if max_row < target_row {
        return None;
    }

    Some(byte_offset_for_buffer_position(
        &st.editor.buffer,
        target_row,
        target_col,
        width,
        left_cols,
    ))
}

fn term_size() -> (usize, usize) {
    raw_term_size()
        .map(|(w, h)| (usize::from(w).max(1), usize::from(h).max(1)))
        .unwrap_or((80, 24))
}

fn raw_term_size() -> io::Result<(u16, u16)> {
    terminal::size()
}

fn resample_resize_dimension(reported: u16, actual: u16) -> u16 {
    if 0 < reported { reported } else { actual }
}

fn effective_resize_dimension(reported: u16, fallback: usize) -> usize {
    let reported = usize::from(reported);
    if 0 < reported {
        reported
    } else {
        fallback.max(1)
    }
}

fn size_event_dimension(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn normalize_paste_text(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }

    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(ch);
        }
    }
    normalized
}

fn is_prompt_line_break(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

fn initial_buffer_position(initial_cols: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    (initial_cols / width, initial_cols % width)
}

fn buffer_position_for_byte(
    s: &str,
    byte_pos: usize,
    width: usize,
    initial_cols: usize,
) -> (usize, usize) {
    let width = width.max(1);
    let mut pos = initial_buffer_position(initial_cols, width);
    let mut pending_exact_wrap = false;

    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(s, true) {
        if byte_pos <= byte || byte_pos < byte + grapheme.len() {
            break;
        }
        advance_prompt_cursor_position(
            &mut pos.0,
            &mut pos.1,
            &mut pending_exact_wrap,
            grapheme,
            width,
        );
    }

    pos
}

fn advance_prompt_cursor_position(
    row: &mut usize,
    col: &mut usize,
    pending_exact_wrap: &mut bool,
    grapheme: &str,
    width: usize,
) {
    let width = width.max(1);
    if is_prompt_line_break(grapheme) {
        if *pending_exact_wrap {
            // A printable character exactly filled the previous visual row, so
            // the cursor is already at column 0 of this row. An explicit newline
            // at that byte position should consume that pending wrap, not add a
            // second blank row.
            *pending_exact_wrap = false;
        } else {
            *row += 1;
            *col = 0;
        }
        return;
    }

    *pending_exact_wrap = false;
    let grapheme_width = display_width(grapheme);
    if 0 < *col && width < *col + grapheme_width {
        *row += 1;
        *col = 0;
    }
    *col += grapheme_width;
    if width <= *col {
        *row += *col / width;
        *col %= width;
        *pending_exact_wrap = grapheme_width != 0 && *col == 0;
    }
}

fn buffer_end_position(s: &str, width: usize, initial_cols: usize) -> (usize, usize) {
    buffer_position_for_byte(s, s.len(), width, initial_cols)
}

fn byte_offset_for_buffer_position(
    s: &str,
    target_row: usize,
    target_col: usize,
    width: usize,
    initial_cols: usize,
) -> usize {
    let mut row_col = initial_buffer_position(initial_cols, width);
    let mut pending_exact_wrap = false;

    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(s, true) {
        let (row, col) = row_col;
        if target_row < row || (target_row == row && target_col <= col) {
            return byte;
        }
        if is_prompt_line_break(grapheme) && !pending_exact_wrap && target_row == row {
            return byte;
        }

        let mut next = row_col;
        let mut next_pending_exact_wrap = pending_exact_wrap;
        advance_prompt_cursor_position(
            &mut next.0,
            &mut next.1,
            &mut next_pending_exact_wrap,
            grapheme,
            width,
        );
        if !is_prompt_line_break(grapheme)
            && (target_row < next.0 || (target_row == next.0 && target_col <= next.1))
        {
            return byte + grapheme.len();
        }
        row_col = next;
        pending_exact_wrap = next_pending_exact_wrap;
    }

    s.len()
}

fn clamp_cursor_to_grapheme_boundary(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    if cursor == s.len() {
        return cursor;
    }

    let mut boundary = 0;
    for (idx, _) in UnicodeSegmentation::grapheme_indices(s, true) {
        if cursor < idx {
            break;
        }
        boundary = idx;
    }
    boundary
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    previous_grapheme_boundary(s, pos)
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    next_grapheme_boundary(s, pos)
}

#[cfg(test)]
mod tests;
