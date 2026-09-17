//! Agent-local values projected by preceding canonical terminal preparation.

/// Read-only overrides for policy evaluation after final-status and
/// continuation preparation. Missing fields retain the current agent value.
#[derive(Clone, Copy, Default)]
pub(super) struct TerminalPolicyProjection {
    /// Prompt-frozen final-status availability, absent for tool terminals.
    pub(super) status_was_available: Option<bool>,
    /// Cursor after any output-length continuation identity consumption.
    pub(super) next_prompt_index: Option<u64>,
}
