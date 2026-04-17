//! Terminal prompt with async output support.
//!
//! Provides a line-editing prompt that can be interrupted by async output
//! without corrupting the display. No alternate screen mode — just
//! diff-based rendering in the normal terminal buffer.
//!
//! # Rendering
//!
//! Uses a dual-buffer diff approach inspired by fish shell's `screen.rs`:
//! the [`screen::Screen`] tracks what is currently on the terminal
//! ("actual") and diffs it against what should be there ("desired"),
//! emitting only the escape sequences for characters that changed. This
//! minimizes terminal I/O, which matters over slow SSH connections.
//!
//! For async output (printed above the prompt), we erase the prompt area
//! using relative cursor movement + `ClearFromCursorDown`, print the
//! output, then let the diff renderer redraw the prompt from scratch
//! (since the actual state was invalidated by the erase).
//!
//! References:
//! - fish shell screen rendering: <https://github.com/fish-shell/fish-shell/blob/master/src/screen.rs>
//! - fish `Screen::update()` for the diff-based repaint logic

pub mod screen;

use std::io::{self, Stdout, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::QueueableCommand;

use screen::{Screen, layout_lines};

/// Events flowing into the UI loop from any producer.
pub enum UiEvent {
    /// A terminal key event from the input thread.
    Key(KeyEvent),
    /// Async output that should be printed above the prompt.
    Output(String),
    /// A request to shut down the UI loop.
    Quit,
}

/// Result of one prompt interaction.
pub enum PromptResult {
    /// The user submitted a line.
    Line(String),
    /// The user signalled EOF (Ctrl-D on empty line).
    Eof,
}

/// A channel-based sender for injecting async output into the prompt.
#[derive(Clone)]
pub struct OutputSender {
    tx: Sender<UiEvent>,
}

impl OutputSender {
    /// Sends an output line to be printed above the prompt.
    pub fn send(&self, text: String) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::Output(text))
    }

    /// Requests the UI loop to shut down.
    pub fn quit(&self) -> Result<(), mpsc::SendError<UiEvent>> {
        self.tx.send(UiEvent::Quit)
    }
}

/// The interactive prompt engine.
pub struct Prompt {
    rx: Receiver<UiEvent>,
    tx: Sender<UiEvent>,
    stdout: Stdout,
    screen: Screen,
    input_handle: Option<JoinHandle<()>>,
    prefix: String,
    buffer: String,
    cursor: usize,
}

impl Prompt {
    /// Creates a new prompt with the given display prefix (e.g. `"> "`).
    ///
    /// Returns the prompt and an [`OutputSender`] that async producers can
    /// use to inject output.
    pub fn new(prefix: &str) -> io::Result<(Self, OutputSender)> {
        let (tx, rx) = mpsc::channel();
        let output_sender = OutputSender { tx: tx.clone() };
        let width = term_width();
        Ok((
            Self {
                rx,
                tx,
                stdout: io::stdout(),
                screen: Screen::new(width),
                input_handle: None,
                prefix: prefix.to_owned(),
                buffer: String::new(),
                cursor: 0,
            },
            output_sender,
        ))
    }

    /// Enters raw mode and starts the input reader thread.
    ///
    /// Must be called before [`read_line`]. Raw mode is exited on
    /// [`Drop`].
    pub fn start(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        self.render()?;

        let tx = self.tx.clone();
        self.input_handle = Some(thread::spawn(move || {
            loop {
                let ev = match event::read() {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                if let CtEvent::Key(key) = ev {
                    if tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
            }
        }));

        Ok(())
    }

    /// Blocks until the user submits a line or signals EOF.
    pub fn read_line(&mut self) -> io::Result<PromptResult> {
        loop {
            let event = match self.rx.recv() {
                Ok(event) => event,
                Err(_) => return Ok(PromptResult::Eof),
            };

            match event {
                UiEvent::Key(key) => {
                    if let Some(result) = self.handle_key(key)? {
                        return Ok(result);
                    }
                }
                UiEvent::Output(text) => {
                    self.print_above(&text)?;
                }
                UiEvent::Quit => {
                    return Ok(PromptResult::Eof);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<Option<PromptResult>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Enter => {
                // Move cursor to end of content so the newline appears
                // after the last character.
                self.cursor = self.buffer.len();
                self.render()?;
                self.stdout.queue(Print("\r\n"))?.flush()?;
                // The prompt area is now above us; invalidate so the next
                // render starts fresh on the new line.
                self.screen.invalidate();
                let line = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                return Ok(Some(PromptResult::Line(line)));
            }

            KeyCode::Char('d') if ctrl => {
                if self.buffer.is_empty() {
                    self.stdout.queue(Print("\r\n"))?.flush()?;
                    self.screen.invalidate();
                    return Ok(Some(PromptResult::Eof));
                }
            }

            KeyCode::Char('c') if ctrl => {
                self.buffer.clear();
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('u') if ctrl => {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('w') if ctrl => {
                if self.cursor > 0 {
                    let before = &self.buffer[..self.cursor];
                    let new_end = before
                        .trim_end()
                        .rfind(' ')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    self.buffer.drain(new_end..self.cursor);
                    self.cursor = new_end;
                    self.render()?;
                }
            }

            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                self.render()?;
            }

            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.render()?;
            }

            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.render()?;
                }
            }

            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let next = next_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(self.cursor..next);
                    self.render()?;
                }
            }

            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.render()?;
                }
            }

            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    self.render()?;
                }
            }

            KeyCode::Home => {
                self.cursor = 0;
                self.render()?;
            }

            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.render()?;
            }

            _ => {}
        }

        Ok(None)
    }

    /// Prints text above the prompt, then redraws the prompt.
    pub fn print_output(&mut self, text: &str) -> io::Result<()> {
        self.print_above(text)
    }

    /// Erases the prompt area, prints text, then redraws the prompt via
    /// the diff renderer (which will emit everything since actual was
    /// invalidated).
    fn print_above(&mut self, text: &str) -> io::Result<()> {
        self.screen.erase_all(&mut self.stdout)?;
        for line in text.lines() {
            self.stdout.queue(Print(line))?.queue(Print("\r\n"))?;
        }
        self.stdout.flush()?;
        // Actual state is gone — next render emits everything.
        self.screen.invalidate();
        self.render()
    }

    /// Builds the desired screen content from prefix + buffer, then diffs
    /// against the actual state via [`Screen::update`].
    fn render(&mut self) -> io::Result<()> {
        self.screen.set_width(term_width());
        let width = self.screen.width();
        let content = format!("{}{}", self.prefix, self.buffer);
        let desired = layout_lines(&content, width);

        // Cursor position in character offset, then map to (row, col).
        let cursor_chars = self.prefix.chars().count() + char_count_for_bytes(&self.buffer, self.cursor);
        let cursor_row = cursor_chars / width;
        let cursor_col = cursor_chars % width;

        self.screen
            .update(&mut self.stdout, &desired, (cursor_row, cursor_col))
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn term_width() -> usize {
    terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(1)
}

/// Returns the number of `char`s in `s[..byte_pos]`.
fn char_count_for_bytes(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos].chars().count()
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_sub(1);
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
