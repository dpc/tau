//! Higher-level terminal prompt with fish-like slash-command completion.
//!
//! Wraps [`tau_cli_term_raw`] and adds:
//!
//! - Slash-command registry with descriptions
//! - Tab-completion menu rendered below the prompt
//! - Tab / Shift-Tab cycling through matches
//! - Inline buffer replacement on selection

use std::io;

use tau_cli_term_raw::Event as RawEvent;
pub use tau_cli_term_raw::{
    Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText, TermHandle,
};

/// A slash command with its name and description.
#[derive(Clone, Debug)]
pub struct SlashCommand {
    /// The command name including the leading `/` (e.g. `"/quit"`).
    pub name: String,
    /// Short description shown in the completion menu.
    pub description: String,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// High-level events surfaced to the caller.
pub enum Event {
    /// The user submitted a line (pressed Enter).
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
    /// The terminal was resized.
    Resize { width: u16, height: u16 },
    /// The input buffer changed.
    BufferChanged,
}

/// Completion state machine.
struct CompletionState {
    /// Filtered matches for the current input prefix.
    matches: Vec<usize>,
    /// Currently highlighted index within `matches` (None = no selection).
    selected: Option<usize>,
    /// The block id used for the completion menu.
    menu_block_id: Option<BlockId>,
    /// The original text the user typed before any completion replacement.
    original_input: String,
}

impl CompletionState {
    fn new() -> Self {
        Self {
            matches: Vec::new(),
            selected: None,
            menu_block_id: None,
            original_input: String::new(),
        }
    }

    fn is_active(&self) -> bool {
        !self.matches.is_empty()
    }

    fn reset(&mut self) {
        self.matches.clear();
        self.selected = None;
        self.original_input.clear();
    }
}

/// Higher-level terminal prompt with completion support.
pub struct HighTerm {
    term: tau_cli_term_raw::Term,
    handle: TermHandle,
    commands: Vec<SlashCommand>,
    completion: CompletionState,
}

impl HighTerm {
    /// Creates a new terminal with the given prompt and slash commands.
    pub fn new(
        left_prompt: impl Into<StyledText>,
        commands: Vec<SlashCommand>,
    ) -> io::Result<(Self, TermHandle)> {
        let (term, handle) = tau_cli_term_raw::Term::new(left_prompt)?;
        let handle_clone = handle.clone();
        Ok((
            Self {
                term,
                handle,
                commands,
                completion: CompletionState::new(),
            },
            handle_clone,
        ))
    }

    /// Returns a clone of the [`TermHandle`] for zone/block manipulation.
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Triggers a redraw.
    pub fn redraw(&self) {
        self.handle.redraw();
    }

    /// Appends persistent output to history.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> BlockId {
        self.handle.print_output(block)
    }

    /// Blocks until the next event, handling completion internally.
    pub fn get_next_event(&mut self) -> io::Result<Event> {
        loop {
            let raw = self.term.get_next_event()?;

            match raw {
                RawEvent::BufferChanged => {
                    self.on_buffer_changed();
                    self.handle.redraw();
                    return Ok(Event::BufferChanged);
                }

                RawEvent::Tab => {
                    if self.completion.is_active() {
                        self.cycle_selection(1);
                        self.handle.redraw();
                    }
                    // If no completions, swallow the Tab.
                    continue;
                }

                RawEvent::BackTab => {
                    if self.completion.is_active() {
                        self.cycle_selection(-1);
                        self.handle.redraw();
                    }
                    continue;
                }

                RawEvent::Escape => {
                    if self.completion.is_active() {
                        self.hide_completion_menu();
                        self.completion.reset();
                        self.handle.redraw();
                    }
                    continue;
                }

                RawEvent::Line(line) => {
                    if self.completion.is_active() && self.completion.selected.is_some() {
                        // A completion was selected — replace the
                        // buffer with it but do NOT submit. The user
                        // can continue editing or press Enter again
                        // to submit.
                        let cmd_idx =
                            self.completion.matches[self.completion.selected.unwrap_or(0)];
                        let cmd_name = self.commands[cmd_idx].name.clone();
                        let cursor = cmd_name.len();
                        self.handle.set_buffer(cmd_name, cursor);
                        self.hide_completion_menu();
                        self.completion.reset();
                        self.handle.redraw();
                        continue;
                    }
                    self.hide_completion_menu();
                    self.completion.reset();
                    self.handle.redraw();
                    return Ok(Event::Line(line));
                }

                RawEvent::Eof => {
                    self.hide_completion_menu();
                    return Ok(Event::Eof);
                }

                RawEvent::Resize { width, height } => {
                    // Rebuild completion menu at new width if active.
                    if self.completion.is_active() {
                        self.render_completion_menu();
                        self.handle.redraw();
                    }
                    return Ok(Event::Resize { width, height });
                }
            }
        }
    }

    /// Called when the buffer content changes. Checks for `/` prefix
    /// and updates the completion list.
    fn on_buffer_changed(&mut self) {
        let buffer = self.handle.get_buffer();

        if !buffer.starts_with('/') || buffer.contains(' ') {
            // Not a slash command or already has arguments — hide completions.
            if self.completion.is_active() {
                self.hide_completion_menu();
                self.completion.reset();
            }
            return;
        }

        // Filter commands matching the prefix.
        let prefix = &buffer;
        let matches: Vec<usize> = self
            .commands
            .iter()
            .enumerate()
            .filter(|(_, cmd)| cmd.name.starts_with(prefix))
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            if self.completion.is_active() {
                self.hide_completion_menu();
                self.completion.reset();
            }
            return;
        }

        self.completion.matches = matches;
        self.completion.selected = None;
        self.completion.original_input = buffer;
        self.render_completion_menu();
    }

    /// Cycles the selection by `delta` (+1 = forward, -1 = backward).
    fn cycle_selection(&mut self, delta: isize) {
        if self.completion.matches.is_empty() {
            return;
        }
        let len = self.completion.matches.len();
        let new_idx = match self.completion.selected {
            None => {
                if delta > 0 {
                    0
                } else {
                    len - 1
                }
            }
            Some(idx) => ((idx as isize + delta).rem_euclid(len as isize)) as usize,
        };
        self.completion.selected = Some(new_idx);
        self.render_completion_menu();
    }

    /// Renders the completion menu as a StyledBlock in the suggestions
    /// zone.
    fn render_completion_menu(&mut self) {
        let selected_style = Style::default().fg(Color::White).bg(Color::DarkBlue).bold();
        let name_style = Style::default().fg(Color::Green);
        let desc_style = Style::default().fg(Color::DarkGrey);

        // Find the longest command name for alignment.
        let max_name_len = self
            .completion
            .matches
            .iter()
            .map(|&i| self.commands[i].name.len())
            .max()
            .unwrap_or(0);

        let mut spans: Vec<Span> = Vec::new();
        for (menu_idx, &cmd_idx) in self.completion.matches.iter().enumerate() {
            if menu_idx > 0 {
                spans.push(Span::plain("\n"));
            }

            let cmd = &self.commands[cmd_idx];
            let is_selected = self.completion.selected == Some(menu_idx);
            let padding = max_name_len - cmd.name.len() + 2;

            let line_text = format!(
                "  {}{:padding$}{}  ",
                cmd.name,
                "",
                cmd.description,
                padding = padding,
            );

            if is_selected {
                spans.push(Span::new(line_text, selected_style));
            } else {
                spans.push(Span::plain("  "));
                spans.push(Span::new(&cmd.name, name_style));
                spans.push(Span::plain(format!("{:padding$}", "", padding = padding)));
                spans.push(Span::new(&cmd.description, desc_style));
                spans.push(Span::plain("  "));
            }
        }

        let block = StyledBlock::new(StyledText::from(spans));
        let block_id = self.ensure_menu_block();
        self.handle.set_block(block_id, block);
    }

    /// Ensures the menu block exists and is in the suggestions zone.
    fn ensure_menu_block(&mut self) -> BlockId {
        if let Some(id) = self.completion.menu_block_id {
            id
        } else {
            let id = self.handle.new_block("");
            self.handle.push_suggestions(id);
            self.completion.menu_block_id = Some(id);
            id
        }
    }

    /// Hides the completion menu by removing the block from
    /// suggestions.
    fn hide_completion_menu(&mut self) {
        if let Some(id) = self.completion.menu_block_id.take() {
            self.handle.remove_suggestions(id);
            self.handle.remove_block(id);
        }
    }
}
