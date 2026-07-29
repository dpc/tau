//! Routes renderer output to the visible terminal or one detached transcript.

use std::sync::Mutex;

use crate::MUTEX_POISONED;

/// Routes transcript mutations either to the visible terminal or to one
/// detached per-agent presentation model.
///
/// Hidden-agent folding selects a detached model, so it neither acquires the
/// terminal output transaction nor clones the visible transcript.
pub(crate) struct RendererHandle {
    /// Handle for the one selected transcript projected into the terminal.
    terminal: tau_cli_term::TermHandle,
    /// Current hidden-agent model while one event folds off-screen.
    detached: Option<Mutex<tau_cli_term::OutputSnapshot>>,
}

impl RendererHandle {
    /// Wraps the one terminal handle owned by the interactive renderer.
    pub(crate) fn new(terminal: tau_cli_term::TermHandle) -> Self {
        Self {
            terminal,
            detached: None,
        }
    }

    /// Selects a detached transcript as the current mutation target.
    pub(crate) fn select_detached(&mut self, snapshot: tau_cli_term::OutputSnapshot) {
        self.detached = Some(Mutex::new(snapshot));
    }

    /// Takes the selected detached transcript and resumes visible mutations.
    pub(crate) fn take_detached(&mut self) -> tau_cli_term::OutputSnapshot {
        self.detached
            .take()
            .expect("detached transcript selected")
            .into_inner()
            .expect(MUTEX_POISONED)
    }

    /// Clones the current target's output model.
    pub(crate) fn output_snapshot(&self) -> tau_cli_term::OutputSnapshot {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).clone()
        } else {
            self.terminal.output_snapshot()
        }
    }

    /// Replaces the current target's complete output model.
    pub(crate) fn replace_output_snapshot(&self, snapshot: tau_cli_term::OutputSnapshot) {
        if let Some(output) = &self.detached {
            *output.lock().expect(MUTEX_POISONED) = snapshot;
        } else {
            self.terminal.replace_output_snapshot(snapshot);
        }
    }

    /// Allocates a styled block in the current target.
    pub(crate) fn new_block(
        &self,
        debug_id: impl Into<String>,
        block: impl Into<tau_cli_term::StyledBlock>,
    ) -> tau_cli_term::BlockId {
        if let Some(output) = &self.detached {
            output
                .lock()
                .expect(MUTEX_POISONED)
                .new_block(debug_id, block)
        } else {
            self.terminal.new_block(debug_id, block)
        }
    }

    /// Replaces one block in the current target.
    pub(crate) fn set_block(
        &self,
        id: tau_cli_term::BlockId,
        block: impl Into<tau_cli_term::StyledBlock>,
    ) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).set_block(id, block);
        } else {
            self.terminal.set_block(id, block);
        }
    }

    /// Removes one block from the current target.
    pub(crate) fn remove_block(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).remove_block(id);
        } else {
            self.terminal.remove_block(id);
        }
    }

    /// Appends a block to the current target's committed history.
    pub(crate) fn push_history(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_history(id);
        } else {
            self.terminal.push_history(id);
        }
    }

    /// Appends a block above the current target's active region.
    pub(crate) fn push_above_active(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_above_active(id);
        } else {
            self.terminal.push_above_active(id);
        }
    }

    /// Inserts a block before the first matching active-region anchor.
    pub(crate) fn push_above_active_before_any<I>(&self, id: tau_cli_term::BlockId, anchors: I)
    where
        I: IntoIterator<Item = tau_cli_term::BlockId>,
    {
        if let Some(output) = &self.detached {
            output
                .lock()
                .expect(MUTEX_POISONED)
                .push_above_active_before_any(id, anchors);
        } else {
            self.terminal.push_above_active_before_any(id, anchors);
        }
    }

    /// Appends a block above the current target's sticky region.
    pub(crate) fn push_above_sticky(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_above_sticky(id);
        } else {
            self.terminal.push_above_sticky(id);
        }
    }

    /// Removes a block from the current target's sticky-region ordering.
    pub(crate) fn remove_above_sticky(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).remove_above_sticky(id);
        } else {
            self.terminal.remove_above_sticky(id);
        }
    }

    /// Appends a block below the current target's active region.
    pub(crate) fn push_below(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_below(id);
        } else {
            self.terminal.push_below(id);
        }
    }

    /// Creates and commits one history block in the current target.
    pub(crate) fn print_output(
        &self,
        debug_id: impl Into<String>,
        block: impl Into<tau_cli_term::StyledBlock>,
    ) -> tau_cli_term::BlockId {
        if let Some(output) = &self.detached {
            output
                .lock()
                .expect(MUTEX_POISONED)
                .print_output(debug_id, block)
        } else {
            self.terminal.print_output(debug_id, block)
        }
    }

    /// Resets the current target to an empty output model.
    pub(crate) fn clear_output(&self) {
        self.replace_output_snapshot(tau_cli_term::OutputSnapshot::default());
    }

    /// Requests a redraw only when the current target is visible.
    pub(crate) fn redraw(&self) {
        if self.detached.is_none() {
            self.terminal.redraw();
        }
    }

    /// Invalidates the screen cache only when the current target is visible.
    pub(crate) fn invalidate_screen(&self) {
        if self.detached.is_none() {
            self.terminal.invalidate_screen();
        }
    }

    /// Clones the visible terminal handle for redraw-suppressed selection
    /// scopes.
    pub(crate) fn terminal_handle(&self) -> tau_cli_term::TermHandle {
        debug_assert!(self.detached.is_none());
        self.terminal.clone()
    }

    /// Returns the visible terminal's completed full-render count.
    pub(crate) fn full_render_count(&self) -> u64 {
        self.terminal.full_render_count()
    }

    /// Returns the visible terminal's current prompt buffer.
    pub(crate) fn get_buffer(&self) -> String {
        self.terminal.get_buffer()
    }

    /// Replaces the visible terminal's left prompt.
    pub(crate) fn set_left_prompt(&self, text: impl Into<tau_cli_term::StyledText>) {
        self.terminal.set_left_prompt(text);
    }

    /// Replaces the visible terminal's right prompt.
    pub(crate) fn set_right_prompt(&self, text: impl Into<tau_cli_term::StyledText>) {
        self.terminal.set_right_prompt(text);
    }

    /// Replaces the visible terminal's empty-input placeholder.
    pub(crate) fn set_input_placeholder(&self, text: impl Into<tau_cli_term::StyledText>) {
        self.terminal.set_input_placeholder(text);
    }

    /// Recalls history before the visible terminal's current prompt text.
    pub(crate) fn recall_prompt_before_current(&self, text: String) {
        self.terminal.recall_prompt_before_current(text);
    }

    /// Enables or disables the visible prompt's scroll indicator.
    pub(crate) fn set_prompt_scroll_indicator(&self, enabled: bool) {
        self.terminal.set_prompt_scroll_indicator(enabled);
    }

    /// Sets the visible terminal's redraw-history line budget.
    pub(crate) fn set_redraw_history_size(&self, size: usize) {
        self.terminal.set_redraw_history_size(size);
    }

    /// Emits a bell through the visible terminal.
    pub(crate) fn print_terminal_bell(&self) {
        self.terminal.print_terminal_bell();
    }

    /// Emits an OSC 1337 user variable through the visible terminal.
    pub(crate) fn print_osc1337_set_user_var(&self, name: &str, value: &str, in_tmux: bool) {
        self.terminal
            .print_osc1337_set_user_var(name, value, in_tmux);
    }
}
