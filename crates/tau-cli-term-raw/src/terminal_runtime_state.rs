//! Owns terminal dimensions, redraw coordination, and lifecycle flags.

/// Terminal dimensions, redraw coordination, and lifecycle flags.
pub(super) struct TerminalRuntimeState {
    /// Current terminal width in columns.
    pub(super) width: usize,
    /// Current terminal height in rows.
    pub(super) height: usize,
    /// Set by `Term::drop` to signal the redraw thread to exit.
    pub(super) shutdown: bool,
    /// Set by another UI owner or virtual input disconnect to ask the blocking
    /// input loop to return sticky EOF.
    pub(super) input_shutdown: bool,
    /// Set while the terminal is released to an external program.
    /// The redraw thread must not write to stdout in this state.
    pub(super) external_paused: bool,
    /// Set by `resume_after_external` (and similar) to force the next redraw to
    /// wipe its `Screen` cache and repaint from scratch. The redraw loop
    /// reads-and-clears this flag.
    pub(super) invalidate_screen: bool,
    /// Latest requested redraw synchronization generation.
    ///
    /// Callers bump this generation; the redraw thread copies it into
    /// `sync_completed` atomically with going idle, immediately before blocking
    /// on receive.
    pub(super) sync_requested: u64,
    /// Latest completed redraw synchronization generation.
    pub(super) sync_completed: u64,
    /// Non-model terminal side effects waiting for the redraw thread's next
    /// pass. Producers use narrow typed APIs on `TermHandle`, so callers cannot
    /// inject arbitrary cursor movement or clear-screen escapes behind the
    /// renderer's back.
    pub(super) pending_raw: Vec<String>,
    /// Nested redraw suppression depth used while the CLI renderer updates an
    /// off-screen agent transcript snapshot.
    pub(super) redraw_suppression: u32,
    /// Whether a redraw arrived while notifications were suppressed. The
    /// outermost suppression guard flushes this request when it drops.
    pub(super) redraw_dirty_while_suppressed: bool,
    /// Maximum persistent-history/log rows replayed during a full redraw.
    /// Older rows are omitted after clearing scrollback so slow terminals do
    /// not receive an unbounded transcript.
    pub(super) redraw_history_size: usize,
    /// Number of full renders performed since creation.
    pub(super) full_render_count: u64,
}

impl TerminalRuntimeState {
    /// Creates terminal runtime state for the supplied dimensions.
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            shutdown: false,
            input_shutdown: false,
            external_paused: false,
            invalidate_screen: false,
            sync_requested: 0,
            sync_completed: 0,
            pending_raw: Vec::new(),
            redraw_suppression: 0,
            redraw_dirty_while_suppressed: false,
            redraw_history_size: usize::MAX,
            full_render_count: 0,
        }
    }
}
