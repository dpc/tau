//! Routes renderer output to the visible terminal or one detached transcript.

use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tau_cli_term::RendererDeliveryId;

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
    /// Whether the current socket handler changed the selected output model.
    selected_delivery_mutated: Cell<bool>,
    /// Whether the current handler should pay presentation-delta accounting.
    selected_delivery_tracking: Cell<bool>,
    /// Test-only override for exercising delta accounting without a subscriber.
    #[cfg(test)]
    force_selected_delivery_tracking: Cell<bool>,
    /// Registered opaque facts retained for focused seam tests.
    #[cfg(test)]
    presentation_observations: RefCell<Vec<TestPresentationObservation>>,
    /// Whether hidden routing disqualifies the current socket handler.
    selected_delivery_suppressed: Cell<bool>,
    /// Renderer-owned redraw requests observed by unit tests.
    #[cfg(test)]
    redraw_request_count: Arc<AtomicU64>,
    /// Output-block replacements observed by focused renderer tests.
    #[cfg(test)]
    block_replacement_count: Cell<u64>,
}

impl RendererHandle {
    /// Wraps the one terminal handle owned by the interactive renderer.
    pub(crate) fn new(terminal: tau_cli_term::TermHandle) -> Self {
        Self {
            terminal,
            detached: None,
            selected_delivery_mutated: Cell::new(false),
            selected_delivery_tracking: Cell::new(false),
            #[cfg(test)]
            force_selected_delivery_tracking: Cell::new(false),
            #[cfg(test)]
            presentation_observations: RefCell::new(Vec::new()),
            selected_delivery_suppressed: Cell::new(false),
            #[cfg(test)]
            redraw_request_count: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            block_replacement_count: Cell::new(0),
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

    /// Transfers the current target's output model by ownership.
    pub(crate) fn take_output_snapshot(&mut self) -> tau_cli_term::OutputSnapshot {
        if let Some(output) = self.detached.take() {
            output.into_inner().expect(MUTEX_POISONED)
        } else {
            self.terminal.take_output_snapshot()
        }
    }

    /// Replaces the current target's complete output model.
    pub(crate) fn replace_output_snapshot(&self, snapshot: tau_cli_term::OutputSnapshot) {
        if let Some(output) = &self.detached {
            *output.lock().expect(MUTEX_POISONED) = snapshot;
        } else {
            self.mark_selected_delivery_mutated();
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
        #[cfg(test)]
        self.block_replacement_count
            .set(self.block_replacement_count.get() + 1);
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).set_block(id, block);
        } else {
            if self.selected_delivery_tracking.get() {
                if self.terminal.set_block_with_presentation_delta(id, block) {
                    self.mark_selected_delivery_mutated();
                }
            } else {
                self.terminal.set_block(id, block);
            }
        }
    }

    /// Removes one block from the current target.
    pub(crate) fn remove_block(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).remove_block(id);
        } else {
            if self.selected_delivery_tracking.get()
                && self.terminal.remove_block_with_presentation_delta(id)
            {
                self.mark_selected_delivery_mutated();
            } else if !self.selected_delivery_tracking.get() {
                self.terminal.remove_block(id);
            }
        }
    }

    /// Appends a block to the current target's committed history.
    pub(crate) fn push_history(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_history(id);
        } else {
            if self.selected_delivery_tracking.get() {
                self.terminal.push_history(id);
                self.mark_selected_delivery_mutated();
            } else {
                self.terminal.push_history(id);
            }
        }
    }

    /// Appends a block above the current target's active region.
    pub(crate) fn push_above_active(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_above_active(id);
        } else {
            if self.selected_delivery_tracking.get()
                && self.terminal.push_above_active_with_presentation_delta(id)
            {
                self.mark_selected_delivery_mutated();
            } else if !self.selected_delivery_tracking.get() {
                self.terminal.push_above_active(id);
            }
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
            if self.selected_delivery_tracking.get() {
                if self
                    .terminal
                    .push_above_active_before_any_with_presentation_delta(id, anchors)
                {
                    self.mark_selected_delivery_mutated();
                }
            } else {
                self.terminal.push_above_active_before_any(id, anchors);
            }
        }
    }

    /// Appends a block below the current target's active region.
    pub(crate) fn push_below(&self, id: tau_cli_term::BlockId) {
        if let Some(output) = &self.detached {
            output.lock().expect(MUTEX_POISONED).push_below(id);
        } else {
            if self.selected_delivery_tracking.get()
                && self.terminal.push_below_with_presentation_delta(id)
            {
                self.mark_selected_delivery_mutated();
            } else if !self.selected_delivery_tracking.get() {
                self.terminal.push_below(id);
            }
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
            self.mark_selected_delivery_mutated();
            self.terminal.print_output(debug_id, block)
        }
    }

    /// Starts mutation accounting for one socket-delivered renderer handler.
    pub(crate) fn begin_selected_delivery(&self, track: bool) {
        self.selected_delivery_mutated.set(false);
        self.selected_delivery_suppressed.set(false);
        #[cfg(test)]
        let track = track || self.force_selected_delivery_tracking.get();
        self.selected_delivery_tracking.set(track);
    }

    /// Forces presentation-delta accounting in focused renderer tests.
    #[cfg(test)]
    pub(crate) fn force_selected_delivery_tracking_for_test(&self) {
        self.force_selected_delivery_tracking.set(true);
    }

    /// Returns whether the current delivery should pay observation costs.
    pub(crate) fn presentation_observation_interest(&self) -> bool {
        let enabled = tracing::enabled!(
            target: "tau_cli_term_raw::frontend_progress",
            tracing::Level::TRACE
        );
        #[cfg(test)]
        let enabled = enabled || self.force_selected_delivery_tracking.get();
        enabled
    }

    /// Registers one already-selected opaque fact with the raw terminal.
    pub(crate) fn observe_presentation_mutation(
        &self,
        delivery_id: RendererDeliveryId,
        class: super::event_renderer::PresentationFactClass,
    ) {
        let fact = class.opaque_fact();
        let _capture_suppressed = self
            .terminal
            .observe_presentation_mutation(delivery_id, fact);
        #[cfg(test)]
        self.presentation_observations
            .borrow_mut()
            .push(TestPresentationObservation {
                delivery_id,
                class,
                fact,
                capture_suppressed: _capture_suppressed,
            });
    }

    /// Returns registered seam observations for focused tests.
    #[cfg(test)]
    pub(crate) fn presentation_observations_for_test(&self) -> Vec<TestPresentationObservation> {
        self.presentation_observations.borrow().clone()
    }

    /// Excludes the current handler because its canonical fact folded
    /// off-screen.
    pub(crate) fn suppress_selected_delivery_observation(&self) {
        self.selected_delivery_suppressed.set(true);
    }

    /// Returns whether the handler completed an eligible selected mutation.
    pub(crate) fn selected_delivery_mutated(&self) -> bool {
        self.selected_delivery_mutated.get() && !self.selected_delivery_suppressed.get()
    }

    /// Marks one selected output-model mutation during the current handler.
    fn mark_selected_delivery_mutated(&self) {
        self.selected_delivery_mutated.set(true);
    }

    /// Requests a redraw only when the current target is visible.
    pub(crate) fn redraw(&self) {
        if self.detached.is_none() {
            #[cfg(test)]
            self.redraw_request_count.fetch_add(1, Ordering::Relaxed);
            self.terminal.redraw();
        }
    }

    /// Returns renderer-owned redraw requests without counting sync barriers.
    #[cfg(test)]
    pub(crate) fn redraw_request_count(&self) -> u64 {
        self.redraw_request_count.load(Ordering::Relaxed)
    }

    /// Returns output-block replacements without inferring them from redraws.
    #[cfg(test)]
    pub(crate) fn block_replacement_count(&self) -> u64 {
        self.block_replacement_count.get()
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
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
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

/// One actual post-fold registration retained by renderer seam tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TestPresentationObservation {
    /// CLI-local delivery identity.
    pub(crate) delivery_id: RendererDeliveryId,
    /// Canonical CLI-owned fact class.
    pub(crate) class: super::event_renderer::PresentationFactClass,
    /// Exact typed opaque fact handed to the raw terminal.
    pub(crate) fact: tau_cli_term::OpaquePresentationFact,
    /// Whether raw capture was suppressed during registration.
    pub(crate) capture_suppressed: bool,
}
