//! Higher-level terminal prompt with slash-command completion.
//!
//! This crate is now a thin shell around [`tau_cli_term_raw`]: the
//! raw layer owns the input state machine (history navigation,
//! completion menu lifecycle, key dispatch). This crate plugs in the
//! *content* (which candidates exist for a given buffer) and the
//! *presentation* (how the menu is rendered as a styled block under
//! the prompt). It also handles `$EDITOR` integration, which doesn't
//! belong in the raw layer.

mod bounded_command;
pub mod completion;
pub mod resolve;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::{Arc, Mutex};

use bounded_command::{ProcessOwnership, run_with_bounded_stdout, run_with_inherited_stdio};
pub use completion::{
    ArgCompleter, CommandName, CompletionData, CompletionItem, CompletionRule, CompletionRules,
    SlashCommand,
};
#[cfg(test)]
pub(crate) use tau_cli_term_raw::RawEvent as TestRawEvent;
pub use tau_cli_term_raw::{
    Align, BlockId, Cell, Color, CursorShape, OutputSnapshot, Span, Style, StyledBlock, StyledText,
    TermHandle,
};
use tau_cli_term_raw::{Candidate, Event as RawEvent};
use tau_themes::Theme;

const PROMPT_TRAILER_MARKER: &str =
    "<!-- TAU trailer: everything after this line will be ignored -->";
// Keep user-facing command limit docs in FEATURES.md and
// docs/cli-keybindings.md in sync with these values.
const PROMPT_COMMAND_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const COMPLETION_COMMAND_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
const COMPLETION_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PROMPT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const PROMPT_HISTORY_SEARCH_MAX_ROWS: usize = 200;
const PROMPT_HISTORY_SUMMARY_MAX_CHARS: usize = 240;
const PROMPT_HISTORY_PREVIEW_MAX_BYTES: usize = 64 * 1024;
const PROMPT_HISTORY_PREVIEW_TOTAL_BYTES: usize = 1024 * 1024;
const COMPLETION_MENU_BLOCK_ID: BlockId = BlockId(u64::MAX);
/// High-level events surfaced to the caller.
pub enum Event {
    /// The user submitted a line (pressed Enter by default, Ctrl-Enter,
    /// or ran `submit-prompt` with no completion preview).
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The user requested prompt cancellation with a second consecutive Ctrl-C.
    CancelPrompt,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The terminal reported focus gained or lost.
    FocusChanged { focused: bool },
    /// The input buffer changed (or the completion menu cycled,
    /// opened, or closed). Caller should redraw any prompt-derived
    /// UI.
    BufferChanged,
    /// Shift+Tab pressed outside an open completion menu.
    BackTab,
    /// Escape pressed outside an open completion menu.
    Escape,
    /// A binding requested an application-defined action without touching the
    /// prompt draft.
    Action(String),
}

/// Higher-level terminal prompt with completion support.
pub struct HighTerm {
    term: tau_cli_term_raw::Term,
    handle: TermHandle,
    theme: Theme,
    editor_context: Arc<Mutex<EditorContext>>,
    /// Editor command resolved once at startup: `$EDITOR`, else
    /// `$VISUAL`, else the first of `hx`/`vim`/`vi`/`nano` found on
    /// `$PATH`. Passed to shell actions as `$TAU_EDITOR`.
    external_editor: Option<String>,
    /// Block id for the completion menu, allocated lazily on first
    /// open. Reused across opens; content swapped to empty when the
    /// menu is hidden.
    menu_block_id: Option<BlockId>,
    /// Submitted prompt history used by prompt-history search. Seeded
    /// from persistent history at startup and extended with submitted
    /// prompts from this process.
    prompt_history: Vec<String>,
    completion_rules: CompletionRules,
    last_command_completion_token: Option<String>,
}

impl HighTerm {
    /// Creates a new terminal with the given prompt and slash commands.
    ///
    /// Returns the terminal, a thread-safe handle for rendering, and a
    /// [`CompletionData`] handle for pushing dynamic argument completions
    /// from background threads.
    pub fn new(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
        cursor_shape: CursorShape,
        bindings: impl IntoIterator<Item = (String, String)>,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        Self::new_with_completion_rules(
            left_prompt,
            commands,
            theme,
            cursor_shape,
            bindings,
            std::iter::empty(),
            CompletionRules::default(),
        )
    }

    /// Creates a new terminal and seeds prompt input history.
    pub fn new_with_input_history(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
        cursor_shape: CursorShape,
        bindings: impl IntoIterator<Item = (String, String)>,
        input_history: impl IntoIterator<Item = String>,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        Self::new_with_completion_rules(
            left_prompt,
            commands,
            theme,
            cursor_shape,
            bindings,
            input_history,
            CompletionRules::default(),
        )
    }

    /// Creates a new terminal with explicit prompt completion rules.
    pub fn new_with_completion_rules(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
        theme: Theme,
        cursor_shape: CursorShape,
        bindings: impl IntoIterator<Item = (String, String)>,
        input_history: impl IntoIterator<Item = String>,
        completion_rules: CompletionRules,
    ) -> io::Result<(Self, TermHandle, CompletionData)> {
        let input_history: Vec<String> = input_history.into_iter().collect();
        let (mut term, handle) = tau_cli_term_raw::Term::new(left_prompt, cursor_shape)?;
        term.seed_input_history(input_history.clone());
        term.set_bindings(bindings);
        let handle_clone = handle.clone();
        let data = CompletionData::new();
        let data_clone = data.clone();
        term.set_completion_source(Some(make_completion_source(
            commands,
            data,
            completion_rules.clone(),
        )));
        let external_editor = resolve_external_editor();
        Ok((
            Self {
                term,
                handle,
                theme,
                editor_context: Arc::new(Mutex::new(EditorContext::default())),
                external_editor,
                menu_block_id: None,
                prompt_history: input_history
                    .into_iter()
                    .filter(|entry| !entry.is_empty())
                    .collect(),
                completion_rules,
                last_command_completion_token: None,
            },
            handle_clone,
            data_clone,
        ))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        mut term: tau_cli_term_raw::Term,
        handle: TermHandle,
        commands: Vec<SlashCommand>,
        theme: Theme,
        bindings: impl IntoIterator<Item = (String, String)>,
    ) -> (Self, CompletionData) {
        let data = CompletionData::new();
        let data_clone = data.clone();
        term.set_completion_source(Some(make_completion_source(
            commands,
            data,
            CompletionRules::default(),
        )));
        term.set_bindings(bindings);
        (
            Self {
                term,
                handle,
                theme,
                editor_context: Arc::new(Mutex::new(EditorContext::default())),
                external_editor: None,
                menu_block_id: None,
                prompt_history: Vec::new(),
                completion_rules: CompletionRules::default(),
                last_command_completion_token: None,
            },
            data_clone,
        )
    }

    /// Returns a reference to the [`TermHandle`].
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Replaces the editor-context storage with a shared handle.
    ///
    /// Use this when another component (e.g. the event renderer) owns
    /// the authoritative context and needs the prompt's external-editor
    /// integration to read conversation context and write prompt-trailer
    /// recovery state through the same `Arc`. The previously-owned
    /// `EditorContext` is dropped.
    /// `EditorContext` is dropped.
    pub fn set_editor_context_handle(&mut self, editor_context: Arc<Mutex<EditorContext>>) {
        self.editor_context = editor_context;
    }

    /// Replaces the prompt UI theme for future local rendering.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.sync_menu_block();
        self.handle.redraw();
    }

    /// Triggers a redraw.
    pub fn redraw(&self) {
        self.handle.redraw();
    }

    /// Closes the active completion menu, if any, and updates its rendered
    /// block.
    ///
    /// Returns `true` when a menu was open. Call this from application-level
    /// state transitions that make the active completion context stale but do
    /// not otherwise change the prompt buffer. Dismissing a previewed candidate
    /// may restore the previous buffer and cursor; this method updates the
    /// rendered menu but does not emit a [`Event::BufferChanged`] event.
    pub fn dismiss_completion_menu(&mut self) -> bool {
        let dismissed = self.term.dismiss_completion_menu();
        if dismissed {
            self.sync_menu_block();
            self.handle.redraw();
        }
        dismissed
    }

    /// Appends persistent output to history.
    pub fn print_output(
        &self,
        debug_id: impl Into<String>,
        block: impl Into<StyledBlock>,
    ) -> BlockId {
        self.handle.print_output(debug_id, block)
    }

    /// Blocks until the next high-level event, syncing the
    /// completion menu block to the raw term's current state.
    pub fn get_next_event(&mut self) -> io::Result<Event> {
        loop {
            let raw = self.term.get_next_event()?;
            match self.handle_next_raw_event(raw) {
                NextEventStep::Return(event) => return Ok(event),
                NextEventStep::Continue => continue,
            }
        }
    }

    fn handle_next_raw_event(&mut self, raw: RawEvent) -> NextEventStep {
        match raw {
            RawEvent::BufferChanged => self.handle_buffer_changed_event(),
            RawEvent::CompletionAccept => self.handle_completion_accept_event(),
            RawEvent::BackTab => NextEventStep::Return(Event::BackTab),
            RawEvent::Escape => NextEventStep::Return(Event::Escape),
            RawEvent::Line(line) => self.handle_line_event(line),
            RawEvent::Eof => {
                self.sync_menu_block();
                NextEventStep::Return(Event::Eof)
            }
            RawEvent::CancelPrompt => {
                self.sync_menu_block();
                self.handle.redraw_sync();
                NextEventStep::Return(Event::CancelPrompt)
            }
            RawEvent::Resize { width, height } => {
                self.sync_menu_block();
                self.handle.redraw();
                NextEventStep::Return(Event::Resize { width, height })
            }
            RawEvent::FocusChanged { focused } => {
                NextEventStep::Return(Event::FocusChanged { focused })
            }
            RawEvent::Notice(message) => self.handle_notice_event(&message),
            RawEvent::ExternalEditor => self.handle_external_editor_event(),
            RawEvent::Binding(action) => self.handle_binding_event(&action),
        }
    }

    fn handle_buffer_changed_event(&mut self) -> NextEventStep {
        if self.maybe_run_command_completion() {
            self.sync_menu_block();
            self.handle.redraw_sync();
            return NextEventStep::Return(Event::BufferChanged);
        }
        self.sync_menu_block();
        self.handle.redraw();
        NextEventStep::Return(Event::BufferChanged)
    }

    fn handle_completion_accept_event(&mut self) -> NextEventStep {
        // Accept-without-submit: the buffer already reflects the chosen
        // candidate. Sync the menu (now closed) and loop so the user has to
        // press Enter again to actually submit.
        self.sync_menu_block();
        self.handle.redraw();
        NextEventStep::Continue
    }

    fn handle_line_event(&mut self, line: String) -> NextEventStep {
        if !line.is_empty() {
            self.prompt_history.push(line.clone());
        }
        self.sync_menu_block();
        self.handle.redraw();
        NextEventStep::Return(Event::Line(line))
    }

    fn handle_notice_event(&mut self, message: &str) -> NextEventStep {
        self.sync_menu_block();
        self.print_local(message);
        self.handle.redraw_sync();
        NextEventStep::Return(Event::BufferChanged)
    }

    fn handle_external_editor_event(&mut self) -> NextEventStep {
        self.sync_menu_block();
        let outcome = self.run_prompt_action(PromptShellAction::Edit(PromptShellCommand {
            command: "$TAU_EDITOR \"$TAU_PROMPT_PATH\"".to_owned(),
            trim: false,
        }));
        self.handle.redraw_sync();
        outcome.into_next_event_step()
    }

    fn handle_binding_event(&mut self, action: &str) -> NextEventStep {
        self.sync_menu_block();
        let outcome = self.run_binding(action);
        self.handle.redraw_sync();
        outcome.into_next_event_step()
    }

    /// Updates the suggestion block to match the raw term's
    /// completion state: renders the menu when one is open, hides
    /// the block otherwise.
    fn sync_menu_block(&mut self) {
        match self.term.completion_state() {
            Some(view) => {
                let (width, height) = self.handle.size();
                let block = completion::render_menu_block(&view, &self.theme, width, height);
                let id = match self.menu_block_id {
                    Some(id) => id,
                    None => {
                        let id = COMPLETION_MENU_BLOCK_ID;
                        self.handle.set_block(id, "");
                        self.handle.push_suggestions(id);
                        self.menu_block_id = Some(id);
                        id
                    }
                };
                self.handle.set_block(id, block);
            }
            None => {
                if let Some(id) = self.menu_block_id.take() {
                    self.handle.remove_suggestions(id);
                    self.handle.remove_block(id);
                }
            }
        }
    }

    fn run_binding(&mut self, action: &str) -> PromptActionOutcome {
        tracing::trace!(target: "tau_cli::input", action, "running prompt binding");
        if tau_cli_term_raw::Term::is_named_action(action) {
            return self
                .term
                .trigger_named_action(action)
                .map_or(PromptActionOutcome::Continue, |raw| {
                    self.apply_raw_prompt_event(raw)
                });
        }
        let Some(action) = PromptShellAction::parse(action) else {
            self.print_local(&format!("binding: unknown action `{action}`"));
            return PromptActionOutcome::BufferChanged;
        };
        self.run_prompt_action(action)
    }

    /// Runs a [`PromptShellAction`] and applies its result to the
    /// input buffer. Errors (spawn failure, bad utf-8, no editor)
    /// surface as a themed info line above the prompt.
    fn run_prompt_action(&mut self, action: PromptShellAction) -> PromptActionOutcome {
        match run_prompt_shell_action(
            &self.term,
            &self.handle,
            self.editor_context.clone(),
            self.external_editor.as_deref(),
            &self.prompt_history,
            action,
        ) {
            Ok(Some(PromptShellResult::Replace(new_text))) => {
                let cursor = new_text.len();
                self.handle.set_buffer(new_text, cursor);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::ReplacePreservingUndo(new_text))) => {
                let cursor = new_text.len();
                self.handle.set_buffer_preserving_undo(new_text, cursor);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Insert(text))) => {
                let mut buffer = self.handle.get_buffer();
                let cursor = self.handle.get_cursor();
                buffer.insert_str(cursor, &text);
                self.handle.set_buffer(buffer, cursor + text.len());
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Action(action))) => {
                return PromptActionOutcome::Return(Event::Action(action));
            }
            Ok(Some(PromptShellResult::History(delta))) => {
                self.term.trigger_history_step(delta);
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Undo)) => {
                self.term.trigger_undo();
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::Redo)) => {
                self.term.trigger_redo();
                self.sync_menu_block();
            }
            Ok(Some(PromptShellResult::RawEvent(raw))) => {
                return self.apply_raw_prompt_event(raw);
            }
            Ok(None) => {} // shell exited non-zero or no output applies.
            Err(msg) => self.print_local(&format!("prompt action: {msg}")),
        }
        PromptActionOutcome::BufferChanged
    }

    fn apply_raw_prompt_event(&mut self, raw: RawEvent) -> PromptActionOutcome {
        match raw {
            RawEvent::BufferChanged => {
                self.sync_menu_block();
                PromptActionOutcome::BufferChanged
            }
            RawEvent::CompletionAccept => {
                self.sync_menu_block();
                PromptActionOutcome::Continue
            }
            RawEvent::Line(line) => {
                if !line.is_empty() {
                    self.prompt_history.push(line.clone());
                }
                self.sync_menu_block();
                PromptActionOutcome::Return(Event::Line(line))
            }
            RawEvent::Eof => PromptActionOutcome::Return(Event::Eof),
            RawEvent::CancelPrompt => PromptActionOutcome::Return(Event::CancelPrompt),
            RawEvent::Resize { width, height } => {
                PromptActionOutcome::Return(Event::Resize { width, height })
            }
            RawEvent::FocusChanged { focused } => {
                PromptActionOutcome::Return(Event::FocusChanged { focused })
            }
            RawEvent::BackTab => PromptActionOutcome::Return(Event::BackTab),
            RawEvent::Escape => PromptActionOutcome::Return(Event::Escape),
            RawEvent::Notice(message) => {
                self.print_local(&message);
                PromptActionOutcome::BufferChanged
            }
            RawEvent::Binding(_) | RawEvent::ExternalEditor => {
                unreachable!("unsupported prompt action event")
            }
        }
    }

    fn maybe_run_command_completion(&mut self) -> bool {
        let buffer = self.handle.get_buffer();
        let cursor = self.handle.get_cursor();
        let Some((command, before, after)) = self
            .completion_rules
            .command_for_exact_token(&buffer, cursor)
            .map(|(command, before, after)| {
                (command.to_vec(), before.to_owned(), after.to_owned())
            })
        else {
            self.last_command_completion_token = None;
            return false;
        };
        let token_key = format!("{before}\0{cursor}");
        if self.last_command_completion_token.as_deref() == Some(token_key.as_str()) {
            return false;
        }
        self.last_command_completion_token = Some(token_key);
        match run_completion_command(&self.term, &command) {
            Ok(Some(text)) => {
                let new_text = format!("{before}{text}{after}");
                let new_cursor = before.len() + text.len();
                self.handle.set_buffer(new_text, new_cursor);
                self.last_command_completion_token = None;
                true
            }
            Ok(None) => false,
            Err(msg) => {
                self.print_local(&format!("completion command: {msg}"));
                true
            }
        }
    }

    fn print_local(&self, message: &str) {
        let block = resolve::themed_block(
            &self.theme,
            tau_themes::names::SYSTEM_INFO,
            message.to_owned(),
        );
        self.handle.print_output("prompt-action-error", block);
    }
}

fn make_completion_source(
    commands: Vec<SlashCommand>,
    data: CompletionData,
    rules: CompletionRules,
) -> Box<dyn tau_cli_term_raw::CompletionSource> {
    let commands = Arc::new(commands);
    let rules = Arc::new(rules);
    Box::new(move |buffer: &str, cursor: usize| -> Vec<Candidate> {
        completion::build_candidates_with_rules(&commands, &data, &rules, buffer, cursor)
    })
}

fn run_completion_command(
    term: &tau_cli_term_raw::Term,
    command: &[String],
) -> Result<Option<String>, String> {
    let Some((program, args)) = command.split_first() else {
        return Err("empty command".to_owned());
    };
    term.pause_for_external()
        .map_err(|e| format!("could not release terminal: {e}"))?;
    struct ResumeGuard<'a>(&'a tau_cli_term_raw::Term);
    impl Drop for ResumeGuard<'_> {
        fn drop(&mut self) {
            if let Err(error) = self.0.resume_after_external() {
                tracing::warn!(target: "tau_cli::input", %error, "failed to resume terminal after completion command");
            }
        }
    }
    let _guard = ResumeGuard(term);
    let mut command_builder = std::process::Command::new(program);
    command_builder
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = run_with_bounded_stdout(
        &mut command_builder,
        None,
        COMPLETION_COMMAND_OUTPUT_LIMIT_BYTES,
        COMPLETION_COMMAND_TIMEOUT,
        ProcessOwnership::ForegroundProcessGroup,
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|e| format!("command output was not utf-8: {e}"))?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

struct PromptShellCommand {
    command: String,
    trim: bool,
}

enum PromptShellAction {
    Insert(PromptShellCommand),
    Edit(PromptShellCommand),
    HistorySearch(PromptShellCommand),
    Action(String),
    PromptNext,
    PromptPrevious,
    PromptUndo,
    PromptRedo,
    SubmitPrompt,
    InsertNewline,
}

/// Conversation context and prompt-editor recovery state appended below the
/// prompt trailer when the user edits the prompt in an external editor.
#[derive(Clone, Default)]
pub struct EditorContext {
    /// Response text currently streaming or otherwise in progress, included as
    /// read-only context when editing the next prompt.
    pub current_response: Option<String>,
    /// Most recent completed response text, included as read-only context when
    /// editing the next prompt.
    pub last_response: Option<String>,
    /// Previous submitted prompt text, included as read-only context when
    /// editing the next prompt.
    pub previous_prompt: Option<String>,
    /// Text recovered from a previous edit where the normally ignored trailer
    /// section was modified before the editor exited. This is set only when the
    /// edited trailer differs from the generated trailer, cleared when the
    /// trailer is unchanged or the marker is deleted, rendered below the marker
    /// on the next edit, and never promoted into the prompt unless the user
    /// manually moves it above the marker.
    pub edited_trailer_recovery: Option<String>,
}

impl EditorContext {
    fn update_edited_trailer_recovery(&mut self, original_text: &str, edited_text: &str) {
        let Some((_, original_trailer)) = split_at_prompt_trailer_marker(original_text) else {
            return;
        };
        self.edited_trailer_recovery = edited_trailer_recovery(original_trailer, edited_text);
    }
}

enum PromptShellResult {
    Insert(String),
    Replace(String),
    ReplacePreservingUndo(String),
    Action(String),
    History(isize),
    Undo,
    Redo,
    RawEvent(RawEvent),
}

#[derive(Clone, Copy)]
enum PromptShellExternalKind {
    Insert,
    Edit,
    HistorySearch,
}

struct PromptShellExternalAction {
    kind: PromptShellExternalKind,
    shell: PromptShellCommand,
}

enum PromptShellDispatch {
    Immediate(PromptShellResult),
    External(PromptShellExternalAction),
}

struct PromptHistoryPicker {
    rows: String,
    prompt_dir: tempfile::TempDir,
}

enum PromptShellCommandOutput {
    Edited,
    Captured(Vec<u8>),
}

enum PromptActionOutcome {
    BufferChanged,
    Continue,
    Return(Event),
}

enum NextEventStep {
    Return(Event),
    Continue,
}

impl PromptActionOutcome {
    fn into_next_event_step(self) -> NextEventStep {
        match self {
            Self::BufferChanged => NextEventStep::Return(Event::BufferChanged),
            Self::Continue => NextEventStep::Continue,
            Self::Return(event) => NextEventStep::Return(event),
        }
    }
}

impl PromptShellAction {
    // Keep prompt-local action names, Term::trigger_named_action,
    // crates/tau-cli/src/chat.rs::encode_binding_action,
    // built-in.cli-bindings.yaml, and docs/cli-keybindings.md in sync.
    fn parse(action: &str) -> Option<Self> {
        match action {
            "prompt-next" => return Some(Self::PromptNext),
            "prompt-previous" => return Some(Self::PromptPrevious),
            "prompt-undo" => return Some(Self::PromptUndo),
            "prompt-redo" => return Some(Self::PromptRedo),
            "submit-prompt" => return Some(Self::SubmitPrompt),
            "insert-newline" => return Some(Self::InsertNewline),
            _ => {}
        }
        let mut parts = action.splitn(3, ':');
        let name = parts.next()?;
        let (Some(mode), Some(command)) = (parts.next(), parts.next()) else {
            return (!action.is_empty() && !action.contains(':'))
                .then(|| Self::Action(action.to_owned()));
        };
        let trim = match mode {
            "trim" => true,
            "raw" => false,
            _ => return None,
        };
        let command = PromptShellCommand {
            command: command.to_owned(),
            trim,
        };
        match name {
            "shell-prompt-insert" => Some(Self::Insert(command)),
            "shell-prompt-edit" => Some(Self::Edit(command)),
            "prompt-history-search" => Some(Self::HistorySearch(command)),
            _ => None,
        }
    }
}

fn prompt_shell_dispatch(
    action: PromptShellAction,
    term: &tau_cli_term_raw::Term,
) -> PromptShellDispatch {
    match action {
        PromptShellAction::PromptNext => {
            PromptShellDispatch::Immediate(PromptShellResult::History(1))
        }
        PromptShellAction::PromptPrevious => {
            PromptShellDispatch::Immediate(PromptShellResult::History(-1))
        }
        PromptShellAction::PromptUndo => PromptShellDispatch::Immediate(PromptShellResult::Undo),
        PromptShellAction::PromptRedo => PromptShellDispatch::Immediate(PromptShellResult::Redo),
        PromptShellAction::Action(action) => {
            PromptShellDispatch::Immediate(PromptShellResult::Action(action))
        }
        PromptShellAction::SubmitPrompt => PromptShellDispatch::Immediate(
            PromptShellResult::RawEvent(term.trigger_submit_or_accept_completion()),
        ),
        PromptShellAction::InsertNewline => PromptShellDispatch::Immediate(
            PromptShellResult::RawEvent(term.trigger_insert_newline()),
        ),
        PromptShellAction::Insert(shell) => {
            PromptShellDispatch::External(PromptShellExternalAction {
                kind: PromptShellExternalKind::Insert,
                shell,
            })
        }
        PromptShellAction::Edit(shell) => {
            PromptShellDispatch::External(PromptShellExternalAction {
                kind: PromptShellExternalKind::Edit,
                shell,
            })
        }
        PromptShellAction::HistorySearch(shell) => {
            PromptShellDispatch::External(PromptShellExternalAction {
                kind: PromptShellExternalKind::HistorySearch,
                shell,
            })
        }
    }
}

fn run_prompt_shell_action(
    term: &tau_cli_term_raw::Term,
    handle: &TermHandle,
    editor_context: Arc<Mutex<EditorContext>>,
    external_editor: Option<&str>,
    prompt_history: &[String],
    action: PromptShellAction,
) -> Result<Option<PromptShellResult>, String> {
    let external_action = match prompt_shell_dispatch(action, term) {
        PromptShellDispatch::Immediate(result) => return Ok(Some(result)),
        PromptShellDispatch::External(external_action) => external_action,
    };
    let current = trim_prompt_newlines(&handle.get_buffer()).to_owned();
    let cursor = handle.get_cursor();
    let tmp = tempfile::Builder::new()
        .prefix("tau-prompt-")
        .suffix(".tau.md")
        .tempfile()
        .map_err(|e| format!("could not create tempfile: {e}"))?;
    let file_text = prompt_shell_file_text(external_action.kind, &current, &editor_context);
    std::fs::write(tmp.path(), file_text.as_bytes())
        .map_err(|e| format!("could not write tempfile: {e}"))?;

    let history_picker = match external_action.kind {
        PromptShellExternalKind::HistorySearch => {
            match prepare_history_picker(term, prompt_history)? {
                Some(history_picker) => Some(history_picker),
                None => return Ok(None),
            }
        }
        PromptShellExternalKind::Insert | PromptShellExternalKind::Edit => None,
    };

    let command = external_action.shell.command.as_str();
    tracing::trace!(
        target: "tau_cli::input",
        command,
        prompt_path = %tmp.path().display(),
        cursor,
        "spawning prompt shell action"
    );
    if command.trim().is_empty() {
        return Err("empty shell command".to_owned());
    }

    term.pause_for_external()
        .map_err(|e| format!("could not release terminal: {e}"))?;
    // RAII so a spawn error / panic still restores raw mode.
    struct ResumeGuard<'a>(&'a tau_cli_term_raw::Term);
    impl Drop for ResumeGuard<'_> {
        fn drop(&mut self) {
            if let Err(error) = self.0.resume_after_external() {
                tracing::warn!(target: "tau_cli::input", %error, "failed to resume terminal after prompt action");
            }
        }
    }
    let _guard = ResumeGuard(term);

    let mut command_builder = prompt_shell_command_builder(
        command,
        tmp.path(),
        cursor,
        external_editor,
        history_picker.as_ref(),
    );
    let Some(output) = run_external_prompt_shell_command(
        &mut command_builder,
        external_action.kind,
        history_picker.as_ref(),
    )?
    else {
        return Ok(None);
    };
    match output {
        PromptShellCommandOutput::Captured(stdout) => {
            return prompt_shell_captured_result(
                external_action.kind,
                external_action.shell.trim,
                stdout,
                prompt_history,
            );
        }
        PromptShellCommandOutput::Edited => {}
    }

    let new_text =
        std::fs::read_to_string(tmp.path()).map_err(|e| format!("could not read tempfile: {e}"))?;
    editor_context
        .lock()
        .expect("editor context mutex poisoned")
        .update_edited_trailer_recovery(&file_text, &new_text);
    let new_text = strip_prompt_trailer(&new_text);
    let new_text = trim_prompt_newlines(new_text).to_owned();
    Ok(Some(PromptShellResult::Replace(new_text)))
}

fn prompt_shell_file_text(
    kind: PromptShellExternalKind,
    current: &str,
    editor_context: &Arc<Mutex<EditorContext>>,
) -> String {
    match kind {
        PromptShellExternalKind::Edit => append_prompt_trailer(current, editor_context),
        PromptShellExternalKind::Insert | PromptShellExternalKind::HistorySearch => {
            current.to_owned()
        }
    }
}

fn prepare_history_picker(
    term: &tau_cli_term_raw::Term,
    prompt_history: &[String],
) -> Result<Option<PromptHistoryPicker>, String> {
    let rows = prompt_history_search_rows(prompt_history);
    if rows.is_empty() {
        return Ok(None);
    }
    let prompt_dir = prompt_history_preview_dir(prompt_history)?;
    term.record_prompt_undo();
    Ok(Some(PromptHistoryPicker { rows, prompt_dir }))
}

fn prompt_shell_command_builder(
    command: &str,
    prompt_path: &std::path::Path,
    cursor: usize,
    external_editor: Option<&str>,
    history_picker: Option<&PromptHistoryPicker>,
) -> std::process::Command {
    let mut command_builder = std::process::Command::new("sh");
    command_builder
        .arg("-c")
        .arg(command)
        .env("TAU_PROMPT_PATH", prompt_path)
        .env("TAU_PROMPT_COLUMN", (cursor + 1).to_string())
        .env("TAU_PROMPT_ROW", "1")
        .env("TAU_EDITOR", external_editor.unwrap_or(""));
    if let Some(history_picker) = history_picker {
        command_builder.env("TAU_PROMPT_HISTORY_DIR", history_picker.prompt_dir.path());
    }
    command_builder
}

fn run_external_prompt_shell_command(
    command_builder: &mut std::process::Command,
    kind: PromptShellExternalKind,
    history_picker: Option<&PromptHistoryPicker>,
) -> Result<Option<PromptShellCommandOutput>, String> {
    match kind {
        PromptShellExternalKind::Edit => run_prompt_edit_command(command_builder),
        PromptShellExternalKind::Insert | PromptShellExternalKind::HistorySearch => {
            run_prompt_capture_command(command_builder, history_picker)
        }
    }
}

fn run_prompt_edit_command(
    command_builder: &mut std::process::Command,
) -> Result<Option<PromptShellCommandOutput>, String> {
    command_builder
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let status = run_with_inherited_stdio(
        command_builder,
        PROMPT_COMMAND_TIMEOUT,
        ProcessOwnership::ForegroundProcessGroup,
    )?
    .status;
    Ok(status.success().then_some(PromptShellCommandOutput::Edited))
}

fn run_prompt_capture_command(
    command_builder: &mut std::process::Command,
    history_picker: Option<&PromptHistoryPicker>,
) -> Result<Option<PromptShellCommandOutput>, String> {
    command_builder.stdin(if history_picker.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    command_builder
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let stdin_input = history_picker.map(|history_picker| history_picker.rows.as_bytes());
    let output = run_with_bounded_stdout(
        command_builder,
        stdin_input,
        PROMPT_COMMAND_OUTPUT_LIMIT_BYTES,
        PROMPT_COMMAND_TIMEOUT,
        ProcessOwnership::ForegroundProcessGroup,
    )?;
    Ok(output
        .status
        .success()
        .then_some(PromptShellCommandOutput::Captured(output.stdout)))
}

fn prompt_shell_captured_result(
    kind: PromptShellExternalKind,
    trim: bool,
    stdout: Vec<u8>,
    prompt_history: &[String],
) -> Result<Option<PromptShellResult>, String> {
    let text =
        String::from_utf8(stdout).map_err(|e| format!("command output was not utf-8: {e}"))?;
    let text = trim_prompt_shell_output(text, trim);
    match kind {
        PromptShellExternalKind::Insert => Ok(Some(PromptShellResult::Insert(text))),
        PromptShellExternalKind::HistorySearch => {
            selected_prompt_history_result(text, prompt_history)
        }
        PromptShellExternalKind::Edit => unreachable!(),
    }
}

fn trim_prompt_shell_output(text: String, trim: bool) -> String {
    if trim { text.trim().to_owned() } else { text }
}

fn selected_prompt_history_result(
    selected: String,
    prompt_history: &[String],
) -> Result<Option<PromptShellResult>, String> {
    let selected_index = selected.split('\t').next().unwrap_or("").trim();
    if selected_index.is_empty() {
        return Ok(None);
    }
    let index = selected_index
        .parse::<usize>()
        .map_err(|e| format!("history selection was not an index: {e}"))?;
    let text = prompt_history
        .get(index)
        .ok_or_else(|| format!("history selection index {index} is out of range"))?
        .clone();
    Ok(Some(PromptShellResult::ReplacePreservingUndo(text)))
}

fn prompt_history_search_rows(prompt_history: &[String]) -> String {
    let mut rows = String::new();
    for (index, prompt) in bounded_prompt_history_entries(prompt_history) {
        rows.push_str(&index.to_string());
        rows.push('\t');
        rows.push_str(&prompt_history_summary(prompt));
        rows.push('\n');
    }
    rows
}

fn prompt_history_preview_dir(prompt_history: &[String]) -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new()
        .prefix("tau-prompt-history-")
        .tempdir()
        .map_err(|e| format!("could not create prompt history tempdir: {e}"))?;
    let mut remaining_total = PROMPT_HISTORY_PREVIEW_TOTAL_BYTES;
    for (index, prompt) in bounded_prompt_history_entries(prompt_history) {
        let preview = bounded_prompt_history_preview(prompt, &mut remaining_total);
        std::fs::write(dir.path().join(index.to_string()), preview.as_bytes())
            .map_err(|e| format!("could not write prompt history preview {index}: {e}"))?;
    }
    Ok(dir)
}

fn bounded_prompt_history_entries(
    prompt_history: &[String],
) -> impl Iterator<Item = (usize, &str)> {
    prompt_history
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, prompt)| !prompt.is_empty())
        .take(PROMPT_HISTORY_SEARCH_MAX_ROWS)
        .map(|(index, prompt)| (index, prompt.as_str()))
}

fn prompt_history_summary(prompt: &str) -> String {
    let mut summary = String::new();
    let mut summary_chars = 0usize;
    let mut pending_space = false;

    for ch in prompt.chars() {
        if ch.is_whitespace() {
            pending_space = !summary.is_empty();
            continue;
        }

        if pending_space {
            if summary_chars + 1 >= PROMPT_HISTORY_SUMMARY_MAX_CHARS {
                append_prompt_history_summary_ellipsis(&mut summary, &mut summary_chars);
                return summary;
            }
            summary.push(' ');
            summary_chars += 1;
            pending_space = false;
        }

        if summary_chars + 1 >= PROMPT_HISTORY_SUMMARY_MAX_CHARS {
            append_prompt_history_summary_ellipsis(&mut summary, &mut summary_chars);
            return summary;
        }
        summary.push(ch);
        summary_chars += 1;
    }

    summary
}

fn append_prompt_history_summary_ellipsis(summary: &mut String, summary_chars: &mut usize) {
    if *summary_chars == PROMPT_HISTORY_SUMMARY_MAX_CHARS {
        summary.pop();
        *summary_chars -= 1;
    }
    summary.push('…');
    *summary_chars += 1;
}

fn bounded_prompt_history_preview(prompt: &str, remaining_total: &mut usize) -> String {
    const TRUNCATED: &str = "\n[history preview truncated]\n";
    if *remaining_total == 0 {
        return String::new();
    }

    let budget = PROMPT_HISTORY_PREVIEW_MAX_BYTES.min(*remaining_total);
    if prompt.len() <= budget {
        *remaining_total = remaining_total.saturating_sub(prompt.len());
        return prompt.to_owned();
    }

    let content_budget = if budget > TRUNCATED.len() {
        budget - TRUNCATED.len()
    } else {
        budget
    };
    let end = previous_char_boundary(prompt, content_budget);
    let mut preview = prompt[..end].to_owned();
    if preview.len() + TRUNCATED.len() <= budget {
        preview.push_str(TRUNCATED);
    }
    *remaining_total = remaining_total.saturating_sub(preview.len());
    preview
}

fn previous_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn append_prompt_trailer(current: &str, editor_context: &Arc<Mutex<EditorContext>>) -> String {
    let context = editor_context
        .lock()
        .expect("editor context mutex poisoned")
        .clone();
    if context.current_response.is_none()
        && context.last_response.is_none()
        && context.previous_prompt.is_none()
        && context.edited_trailer_recovery.is_none()
    {
        return current.to_owned();
    }

    let mut out = trim_prompt_newlines(current).to_owned();
    out.push_str("\n\n");
    out.push_str(PROMPT_TRAILER_MARKER);
    out.push('\n');
    if let Some(text) = context
        .current_response
        .as_deref()
        .filter(|t| !t.is_empty())
    {
        out.push_str("\n## Current response in progress\n\n");
        push_markdown_quote(&mut out, text);
    }
    if let Some(text) = context.last_response.as_deref().filter(|t| !t.is_empty()) {
        out.push_str("\n## Last response\n\n");
        push_markdown_quote(&mut out, text);
    }
    if let Some(text) = context.previous_prompt.as_deref().filter(|t| !t.is_empty()) {
        out.push_str("\n## Previous prompt\n\n");
        push_markdown_quote(&mut out, text);
    }
    if let Some(text) = context
        .edited_trailer_recovery
        .as_deref()
        .filter(|t| !t.is_empty())
    {
        out.push_str("\n## Previously edited text below TAU trailer\n\n");
        out.push_str(
            "Move anything you want to keep above the TAU trailer marker; \
             leaving this section unchanged will discard it after this editor session.\n\n",
        );
        out.push_str(text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn trim_prompt_newlines(text: &str) -> &str {
    text.trim_matches(['\n', '\r'])
}

fn strip_prompt_trailer(text: &str) -> &str {
    let Some((before, _)) = split_at_prompt_trailer_marker(text) else {
        return text;
    };
    trim_prompt_before_trailer_marker(before)
}

fn edited_trailer_recovery(original_trailer: &str, edited_text: &str) -> Option<String> {
    let (_, edited_trailer) = split_at_prompt_trailer_marker(edited_text)?;
    if edited_trailer == original_trailer {
        return None;
    }
    let trimmed = trim_prompt_newlines(edited_trailer);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn trim_prompt_before_trailer_marker(before: &str) -> &str {
    before
        .strip_suffix("\n\n")
        .or_else(|| before.strip_suffix("\r\n\r\n"))
        .or_else(|| before.strip_suffix('\n'))
        .or_else(|| before.strip_suffix("\r\n"))
        .unwrap_or(before)
}

fn split_at_prompt_trailer_marker(text: &str) -> Option<(&str, &str)> {
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        let line_without_ending = line_without_newline
            .strip_suffix('\r')
            .unwrap_or(line_without_newline);
        if line_without_ending == PROMPT_TRAILER_MARKER {
            let trailer_start = line_start + line.len();
            return Some((&text[..line_start], &text[trailer_start..]));
        }
        line_start += line.len();
    }
    None
}

fn push_markdown_quote(out: &mut String, text: &str) {
    for line in text.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
}

/// Resolves the external editor once at startup: `$EDITOR`, then `$VISUAL`,
/// then the first of `hx`/`vim`/`vi`/`nano` found on `$PATH`. The resolved
/// command is exposed to prompt shell actions as `$TAU_EDITOR`.
fn resolve_external_editor() -> Option<String> {
    for var in ["EDITOR", "VISUAL"] {
        if let Some(val) = std::env::var_os(var) {
            let s = val.to_string_lossy();
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    ["hx", "vim", "vi", "nano"]
        .into_iter()
        .find(|cand| which::which(cand).is_ok())
        .map(str::to_owned)
}
