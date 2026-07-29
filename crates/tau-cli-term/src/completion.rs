//! Command-mode and argument completion content + menu rendering.
//!
//! State and lifecycle live in [`tau_cli_term_raw`]; this module
//! supplies the *content* (which candidates exist for a given buffer)
//! and the *presentation* (how the menu block is laid out and styled).
//!
//! Public types:
//! - [`CommandCompletion`] — command completion entry
//! - [`CompletionItem`] / [`CompletionData`] — dynamic argument completions
//! - [`build_candidates`] — turns the current buffer into a `Vec<Candidate>`
//! - [`render_menu_block`] — turns a [`CompletionView`] into a [`StyledBlock`]

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tau_cli_term_raw::{Candidate, CompletionView, Span, StyledBlock, StyledText};
use tau_term_screen::{display_width, truncate_to_width};
use tau_themes::Theme;

use crate::resolve;

mod git_files;

/// One command-mode token, including its leading `:` (for example, `":model"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandName(String);

impl CommandName {
    /// Creates a command name and asserts it follows the command-token grammar.
    ///
    /// # Panics
    ///
    /// Panics unless `name` is one colon followed by an ASCII alphanumeric
    /// character and then only ASCII alphanumeric, `_`, or `-` characters.
    pub fn new(name: impl Into<String>) -> Self {
        let s = name.into();
        assert!(
            is_valid_command_name(&s),
            "CommandName must be ':' followed by one command token"
        );
        Self(s)
    }

    /// Returns the command name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One command-mode completion entry with its display description.
#[derive(Clone, Debug)]
pub struct CommandCompletion {
    /// Command token typed by the user, including the leading `:`.
    pub name: CommandName,
    /// Human-readable description shown in the completion menu.
    pub description: String,
}

impl CommandCompletion {
    /// Creates a command-mode entry with a display description for completion
    /// menus.
    ///
    /// # Panics
    ///
    /// Panics when `name` does not satisfy [`CommandName::new`].
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: CommandName::new(name),
            description: description.into(),
        }
    }
}

pub(crate) fn is_valid_command_name(name: &str) -> bool {
    let Some(token) = name.strip_prefix(':') else {
        return false;
    };
    let mut chars = token.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// A single argument completion candidate.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    /// Text inserted into the prompt when this completion is accepted.
    pub value: String,
    /// Human-readable description shown beside the value in the menu.
    pub description: String,
}

impl CompletionItem {
    /// Creates an argument completion with a menu description.
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }

    /// Creates an argument completion with no menu description.
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: String::new(),
        }
    }
}

/// Closure that produces argument completions for a command,
/// given the already-typed args (the last element is the partial arg
/// being completed; may be empty for "just typed a space").
///
/// The closure is responsible for filtering and ranking — callers do
/// no further processing. For the common flat-list case use
/// [`CompletionData::set_arg_completions`], which builds an appropriate
/// closure internally.
pub type ArgCompleter = Arc<dyn Fn(&[&str]) -> Vec<CompletionItem> + Send + Sync>;

/// Mutable completion state shared with background renderer updates.
#[derive(Default)]
struct CompletionInner {
    arg_completers: HashMap<CommandName, ArgCompleter>,
    dynamic_arg_completers: HashMap<CommandName, ArgCompleter>,
    dynamic_commands: Vec<CommandCompletion>,
    agent_mention_completer: Option<ArgCompleter>,
}

/// Thread-safe storage for dynamic command and argument completions.
///
/// Clone this handle and pass it to background threads that need to
/// update available completions (e.g. when the harness sends a model
/// list or an extension publishes an action schema).
#[derive(Clone, Default)]
pub struct CompletionData {
    inner: Arc<Mutex<CompletionInner>>,
}

impl CompletionData {
    /// Creates empty, shareable dynamic completion storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces extension-provided root commands shown alongside the
    /// static command registry.
    pub fn set_dynamic_commands(&self, commands: Vec<CommandCompletion>) {
        self.set_dynamic_commands_and_arg_completers(commands, Vec::new());
    }

    /// Replaces extension-provided root commands and their nested
    /// argument/subcommand completers as one atomic snapshot.
    pub fn set_dynamic_commands_and_arg_completers(
        &self,
        commands: Vec<CommandCompletion>,
        arg_completers: Vec<(CommandName, ArgCompleter)>,
    ) {
        let mut inner = self.inner.lock().expect("completion data lock");
        inner.dynamic_commands = commands;
        inner.dynamic_arg_completers = arg_completers.into_iter().collect();
    }

    /// Sets a flat, single-arg completion list for a command.
    /// Items are ranked prefix-match-first, substring-match-second
    /// (case-insensitive). For commands that take more than one arg
    /// or need to react to prior args, use
    /// [`CompletionData::set_arg_completer`].
    pub fn set_arg_completions(&self, command: CommandName, items: Vec<CompletionItem>) {
        // Precompute lowercased haystacks once at insertion time so
        // the per-keystroke match loop doesn't reallocate.
        let indexed: Arc<Vec<(CompletionItem, String)>> = Arc::new(
            items
                .into_iter()
                .map(|item| {
                    let lower = item.value.to_lowercase();
                    (item, lower)
                })
                .collect(),
        );
        let completer: ArgCompleter = Arc::new(move |args: &[&str]| {
            // Single-arg completion only — multi-arg buffers fall
            // through to no candidates.
            if args.len() != 1 {
                return Vec::new();
            }
            let needle = args[0].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for (item, value_lower) in indexed.iter() {
                if needle.is_empty() || value_lower.starts_with(&needle) {
                    prefix_matches.push(item.clone());
                } else if value_lower.contains(&needle) {
                    substr_matches.push(item.clone());
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        });
        self.inner
            .lock()
            .expect("completion data lock")
            .arg_completers
            .insert(command, completer);
    }

    /// Registers a custom argument completer for a command.
    /// The closure receives the args typed so far (with the partial
    /// last element being completed) and returns ranked candidates.
    pub fn set_arg_completer(&self, command: CommandName, completer: ArgCompleter) {
        self.inner
            .lock()
            .expect("completion data lock")
            .arg_completers
            .insert(command, completer);
    }

    fn get_arg_completer(&self, command: &CommandName) -> Option<ArgCompleter> {
        let inner = self.inner.lock().expect("completion data lock");
        inner
            .arg_completers
            .get(command)
            .or_else(|| inner.dynamic_arg_completers.get(command))
            .cloned()
    }

    /// Registers prompt-text completion for active agent mentions typed as
    /// `@<partial-agent-id>`.
    pub fn set_agent_mention_completer(&self, completer: ArgCompleter) {
        self.inner
            .lock()
            .expect("completion data lock")
            .agent_mention_completer = Some(completer);
    }

    fn get_agent_mention_completer(&self) -> Option<ArgCompleter> {
        self.inner
            .lock()
            .expect("completion data lock")
            .agent_mention_completer
            .clone()
    }

    fn dynamic_commands(&self) -> Vec<CommandCompletion> {
        self.inner
            .lock()
            .expect("completion data lock")
            .dynamic_commands
            .clone()
    }
}

/// Named completion behavior selected by prompt completion config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionRuleKind {
    /// Complete active agent mentions from harness-provided agent data.
    Agents,
    /// Complete filesystem paths by reading the matching directory.
    Path,
    /// Complete filesystem paths, preferring fuzzy git-tracked file matches for
    /// `./<partial>` inside a repository.
    PathFuzzy,
    /// Complete action/command names.
    Actions,
    /// Run an external command when the trigger token is typed exactly.
    Command(Vec<String>),
}

/// A single prompt completion rule keyed by the word prefix that activates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRule {
    /// Word prefix that activates this completion rule.
    pub prefix: String,
    /// Completion behavior selected for the prefix.
    pub kind: CompletionRuleKind,
}

impl CompletionRule {
    /// Parses a `cli.yaml` completion entry such as `complete_path` or
    /// `complete_with_command fzf --filter foo`.
    pub fn parse(prefix: impl Into<String>, spec: &str) -> Option<Self> {
        let prefix = prefix.into();
        let mut parts = spec.split_whitespace();
        let name = parts.next()?;
        let kind = match name {
            "complete_agents" => CompletionRuleKind::Agents,
            "complete_path" => CompletionRuleKind::Path,
            "complete_path_fuzzy" => CompletionRuleKind::PathFuzzy,
            "complete_actions" => CompletionRuleKind::Actions,
            "complete_with_command" => {
                let args = parts.map(ToOwned::to_owned).collect::<Vec<_>>();
                if args.is_empty() {
                    return None;
                }
                CompletionRuleKind::Command(args)
            }
            _ => return None,
        };
        Some(Self { prefix, kind })
    }
}

/// Prompt completion rules. If multiple rules match, the longest prefix wins.
#[derive(Clone, Debug)]
pub struct CompletionRules {
    rules: Vec<CompletionRule>,
}

impl CompletionRules {
    /// Creates rules sorted so the longest matching prefix wins.
    pub fn new(mut rules: Vec<CompletionRule>) -> Self {
        rules.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then(a.prefix.cmp(&b.prefix))
        });
        Self { rules }
    }

    /// Built-in prompt completion defaults used when no config is supplied.
    pub fn built_in() -> Self {
        Self::new(vec![
            CompletionRule::parse("@", "complete_agents").expect("valid built-in completion"),
            CompletionRule::parse("./", "complete_path").expect("valid built-in completion"),
            CompletionRule::parse("../", "complete_path").expect("valid built-in completion"),
            CompletionRule::parse("/", "complete_path").expect("valid built-in completion"),
            CompletionRule::parse("~", "complete_path").expect("valid built-in completion"),
            CompletionRule::parse("~/", "complete_path").expect("valid built-in completion"),
        ])
    }

    fn matching_rule(&self, token_prefix: &str) -> Option<&CompletionRule> {
        self.rules
            .iter()
            .find(|rule| token_prefix.starts_with(&rule.prefix))
    }

    /// Returns the argv and replacement surroundings for an exact command
    /// trigger token at the cursor.
    pub fn command_for_exact_token<'a>(
        &'a self,
        buffer: &'a str,
        cursor: usize,
    ) -> Option<(&'a [String], &'a str, &'a str)> {
        if first_non_whitespace_starts_command(buffer) {
            return None;
        }
        let token = word_token(buffer, cursor)?;
        if buffer
            .get(cursor..)?
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace())
        {
            return None;
        }
        let rule = self.rules.iter().find(|rule| rule.prefix == token.prefix)?;
        match &rule.kind {
            CompletionRuleKind::Command(command) => {
                Some((command.as_slice(), token.before, token.after))
            }
            _ => None,
        }
    }
}

impl Default for CompletionRules {
    fn default() -> Self {
        Self::built_in()
    }
}

/// Builds the candidate list for the given buffer/cursor.
pub fn build_candidates(
    commands: &[CommandCompletion],
    data: &CompletionData,
    buffer: &str,
    cursor: usize,
) -> Vec<Candidate> {
    build_candidates_with_rules(commands, data, &CompletionRules::default(), buffer, cursor)
}

/// Builds candidates using explicit prompt completion rules.
pub fn build_candidates_with_rules(
    commands: &[CommandCompletion],
    data: &CompletionData,
    rules: &CompletionRules,
    buffer: &str,
    cursor: usize,
) -> Vec<Candidate> {
    build_candidates_with_home_and_rules(
        commands,
        data,
        rules,
        buffer,
        cursor,
        home_dir().as_deref(),
    )
}

#[cfg(test)]
pub(crate) fn build_candidates_with_home(
    commands: &[CommandCompletion],
    data: &CompletionData,
    buffer: &str,
    cursor: usize,
    home_dir: Option<&Path>,
) -> Vec<Candidate> {
    build_candidates_with_home_and_rules(
        commands,
        data,
        &CompletionRules::default(),
        buffer,
        cursor,
        home_dir,
    )
}

pub(crate) fn build_candidates_with_home_and_rules(
    commands: &[CommandCompletion],
    data: &CompletionData,
    rules: &CompletionRules,
    buffer: &str,
    cursor: usize,
    home_dir: Option<&Path>,
) -> Vec<Candidate> {
    if first_non_whitespace_starts_command(buffer) {
        let leading_len = buffer.len() - buffer.trim_start().len();
        let view = &buffer[leading_len..];
        if cursor < leading_len {
            return Vec::new();
        }
        let view_cursor = clamp_to_char_boundary(view, cursor.saturating_sub(leading_len));
        if view_cursor == 0 {
            return Vec::new();
        }
        let command_token_end = first_whitespace(view)
            .map(|(index, _)| index)
            .unwrap_or(view.len());
        if view_cursor <= command_token_end {
            let prefix = &view[..view_cursor];
            let suffix = &view[command_token_end..];
            let candidates = build_cmd_candidates(commands, &data.dynamic_commands(), prefix);
            return replace_token_candidates(&buffer[..leading_len], suffix, candidates);
        }

        if let Some((space_pos, space_ch)) = first_whitespace(view) {
            let cmd = &view[..space_pos];
            if cmd.is_empty() {
                return Vec::new();
            }
            let rest_start = space_pos + space_ch.len_utf8();
            let rest = &view[rest_start..];
            let rest_cursor = view_cursor.saturating_sub(rest_start).min(rest.len());
            let candidates = build_arg_candidates(data, cmd, rest, rest_cursor);
            return prepend_to_replacements(&buffer[..leading_len], candidates);
        }
    }

    let Some(token) = word_token(buffer, cursor) else {
        return Vec::new();
    };
    let Some(rule) = rules.matching_rule(token.prefix) else {
        return Vec::new();
    };

    match &rule.kind {
        CompletionRuleKind::Agents => build_agent_mention_candidates(data, &token, &rule.prefix),
        CompletionRuleKind::Path => build_filesystem_candidates_with_home(&token, home_dir, false),
        CompletionRuleKind::PathFuzzy => {
            build_filesystem_candidates_with_home(&token, home_dir, true)
        }
        CompletionRuleKind::Actions => {
            build_action_token_candidates(commands, &data.dynamic_commands(), &token, &rule.prefix)
        }
        CompletionRuleKind::Command(_) => Vec::new(),
    }
}

fn build_cmd_candidates(
    static_commands: &[CommandCompletion],
    dynamic_commands: &[CommandCompletion],
    prefix: &str,
) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    static_commands
        .iter()
        .chain(dynamic_commands)
        .filter(|cmd| seen.insert(cmd.name.to_string()))
        .filter(|cmd| cmd.name.as_str().starts_with(prefix))
        .map(|cmd| Candidate {
            label: cmd.name.to_string(),
            description: cmd.description.clone(),
            replacement: cmd.name.to_string(),
            cursor: cmd.name.as_str().len(),
        })
        .collect()
}
fn prepend_to_replacements(prefix: &str, candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates
        .into_iter()
        .map(|candidate| Candidate {
            replacement: format!("{prefix}{}", candidate.replacement),
            cursor: prefix.len() + candidate.cursor,
            ..candidate
        })
        .collect()
}

fn replace_token_candidates(
    before: &str,
    after: &str,
    candidates: Vec<Candidate>,
) -> Vec<Candidate> {
    candidates
        .into_iter()
        .map(|candidate| {
            let accepted = candidate.replacement.clone();
            replace_candidate(candidate, before, &accepted, after)
        })
        .collect()
}

fn replace_candidate(candidate: Candidate, before: &str, accepted: &str, after: &str) -> Candidate {
    Candidate {
        replacement: format!("{before}{accepted}{after}"),
        cursor: before.len() + accepted.len(),
        ..candidate
    }
}

fn build_action_token_candidates(
    static_commands: &[CommandCompletion],
    dynamic_commands: &[CommandCompletion],
    token: &PathToken<'_>,
    trigger_prefix: &str,
) -> Vec<Candidate> {
    let partial = token
        .prefix
        .strip_prefix(trigger_prefix)
        .unwrap_or(token.prefix);
    let lookup_prefix = if trigger_prefix == ":" {
        token.prefix.to_owned()
    } else {
        format!(":{partial}")
    };
    build_cmd_candidates(static_commands, dynamic_commands, &lookup_prefix)
        .into_iter()
        .map(|candidate| {
            let replacement = if trigger_prefix == ":" {
                candidate.replacement.clone()
            } else {
                format!(
                    "{trigger_prefix}{}",
                    candidate.replacement.trim_start_matches(':')
                )
            };
            replace_candidate(candidate, token.before, &replacement, token.after)
        })
        .collect()
}
struct PathToken<'a> {
    prefix: &'a str,
    before: &'a str,
    after: &'a str,
}

fn first_non_whitespace_starts_command(buffer: &str) -> bool {
    let trimmed = buffer.trim_start();
    trimmed.starts_with(':') && !trimmed.starts_with("::")
}

fn word_token(buffer: &str, cursor: usize) -> Option<PathToken<'_>> {
    let before_cursor = buffer.get(..cursor)?;
    let after_cursor = buffer.get(cursor..)?;
    let token_start = before_cursor
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx + ch.len_utf8()))
        .unwrap_or(0);
    let token_end = after_cursor
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(cursor + idx))
        .unwrap_or(buffer.len());
    Some(PathToken {
        prefix: &buffer[token_start..cursor],
        before: &buffer[..token_start],
        after: &buffer[token_end..],
    })
}
fn build_agent_mention_candidates(
    data: &CompletionData,
    token: &PathToken<'_>,
    trigger_prefix: &str,
) -> Vec<Candidate> {
    let Some(completer) = data.get_agent_mention_completer() else {
        return Vec::new();
    };
    let partial = token
        .prefix
        .strip_prefix(trigger_prefix)
        .unwrap_or(token.prefix);
    completer(&[partial])
        .into_iter()
        .map(|item| {
            let accepted = format!("{trigger_prefix}{}", item.value);
            replace_candidate(
                Candidate {
                    label: item.value,
                    description: item.description,
                    replacement: String::new(),
                    cursor: 0,
                },
                token.before,
                &accepted,
                token.after,
            )
        })
        .collect()
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.as_os_str().is_empty() {
        None
    } else {
        Some(PathBuf::from(home))
    }
}
fn home_expanded_path(prefix: &str, home_dir: Option<&Path>) -> Option<PathBuf> {
    if prefix == "~" {
        Some(home_dir?.to_path_buf())
    } else if let Some(rest) = prefix.strip_prefix("~/") {
        Some(home_dir?.join(rest))
    } else {
        Some(PathBuf::from(prefix))
    }
}

fn build_filesystem_candidates_with_home(
    path_token: &PathToken<'_>,
    home_dir: Option<&Path>,
    fuzzy_git_files: bool,
) -> Vec<Candidate> {
    let prefix = path_token.prefix;
    let Some(lookup_path) = home_expanded_path(prefix, home_dir) else {
        return Vec::new();
    };
    let display_path = Path::new(prefix);
    let (lookup_dir, display_dir, partial) = if prefix == "~" {
        (lookup_path, PathBuf::from("~"), "")
    } else if prefix.ends_with('/') {
        (lookup_path, display_path.to_path_buf(), "")
    } else {
        let Some(lookup_parent) = lookup_path.parent() else {
            return Vec::new();
        };
        let Some(display_parent) = display_path.parent() else {
            return Vec::new();
        };
        let partial = display_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let lookup_dir = if lookup_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            lookup_parent.to_path_buf()
        };
        let display_dir = if display_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            display_parent.to_path_buf()
        };
        (lookup_dir, display_dir, partial)
    };

    if fuzzy_git_files && prefix.starts_with("./") && !partial.is_empty() {
        let cwd = cwd();
        if let Some((repo_root, files)) = git_files::git_repo_files(&cwd) {
            let matches = git_files::fuzzy_match_git_files(partial, &files);
            if !matches.is_empty() {
                return matches
                    .into_iter()
                    .map(|path| {
                        let display = git_files::dotslash_display_path(path, &repo_root, &cwd);
                        replace_candidate(
                            Candidate {
                                label: display.clone(),
                                description: "git file".to_owned(),
                                replacement: String::new(),
                                cursor: 0,
                            },
                            path_token.before,
                            &display,
                            path_token.after,
                        )
                    })
                    .collect();
            }
        }
    }

    let Ok(entries) = std::fs::read_dir(lookup_dir) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(partial) {
            continue;
        }
        if !partial.starts_with('.') && name.starts_with('.') {
            continue;
        }

        let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
        let mut replacement = display_dir.join(name).to_string_lossy().into_owned();
        if is_dir && !replacement.ends_with('/') {
            replacement.push('/');
        }
        candidates.push(replace_candidate(
            Candidate {
                label: replacement.clone(),
                description: if is_dir { "directory" } else { "file" }.to_owned(),
                replacement: String::new(),
                cursor: 0,
            },
            path_token.before,
            &replacement,
            path_token.after,
        ));
    }

    candidates.sort_by(|a, b| a.label.cmp(&b.label));
    candidates
}

fn clamp_to_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while 0 < cursor && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn first_whitespace(text: &str) -> Option<(usize, char)> {
    text.char_indices().find(|(_, ch)| ch.is_whitespace())
}

fn build_arg_candidates(
    data: &CompletionData,
    cmd: &str,
    rest: &str,
    rest_cursor: usize,
) -> Vec<Candidate> {
    let cmd_name = CommandName::new(cmd);
    let Some(completer) = data.get_arg_completer(&cmd_name) else {
        return Vec::new();
    };

    let rest_cursor = clamp_to_char_boundary(rest, rest_cursor);
    let token_start = rest[..rest_cursor]
        .char_indices()
        .rev()
        .find_map(|(pos, ch)| ch.is_whitespace().then_some(pos + ch.len_utf8()))
        .unwrap_or(0);
    let token_end = rest[rest_cursor..]
        .find(char::is_whitespace)
        .map(|pos| rest_cursor + pos)
        .unwrap_or(rest.len());

    let mut args: Vec<&str> = rest[..token_start].split_whitespace().collect();
    args.push(&rest[token_start..rest_cursor]);

    let replacement_prefix = format!("{cmd} {}", &rest[..token_start]);
    let replacement_suffix = &rest[token_end..];

    completer(&args)
        .into_iter()
        .map(|item| {
            let accepted = item.value.clone();
            replace_candidate(
                Candidate {
                    label: item.value,
                    description: item.description,
                    replacement: String::new(),
                    cursor: 0,
                },
                &replacement_prefix,
                &accepted,
                replacement_suffix,
            )
        })
        .collect()
}

const COMPLETION_MENU_MAX_HEIGHT_PERCENT: usize = 30;

/// Renders the completion menu as a [`StyledBlock`]: each candidate
/// on its own line, with the selected entry highlighted.
pub fn render_menu_block(
    view: &CompletionView,
    theme: &Theme,
    terminal_width: usize,
    terminal_height: usize,
) -> StyledBlock {
    render_menu_block_with_max_rows(
        view,
        theme,
        terminal_width,
        completion_menu_max_rows(terminal_height),
    )
}

fn completion_menu_max_rows(terminal_height: usize) -> usize {
    (terminal_height * COMPLETION_MENU_MAX_HEIGHT_PERCENT / 100).max(1)
}

fn visible_candidate_range(view: &CompletionView, max_rows: usize) -> std::ops::Range<usize> {
    let total = view.candidates.len();
    let max_rows = max_rows.max(1).min(total.max(1));
    if total <= max_rows {
        return 0..total;
    }

    let selected = view.selected.unwrap_or(0).min(total - 1);
    let half = max_rows / 2;
    let start = selected.saturating_sub(half).min(total - max_rows);
    start..start + max_rows
}

struct MenuLineParts {
    label: String,
    padding: usize,
    description: String,
}

fn menu_line_parts(
    candidate: &Candidate,
    max_label_width: usize,
    terminal_width: usize,
) -> MenuLineParts {
    let inner_width = if terminal_width < 4 {
        terminal_width.max(1)
    } else {
        terminal_width.max(1).saturating_sub(4)
    };
    let label_budget = max_label_width.min(inner_width);
    let label = truncate_to_width(&candidate.label, label_budget);
    let label_width = display_width(label.as_str());
    let remaining = inner_width.saturating_sub(label_width);

    let mut padding = 0;
    let mut description = String::new();
    if !candidate.description.is_empty() && 0 < remaining {
        padding = (max_label_width.saturating_sub(label_width) + 2).min(remaining);
        let desc_budget = remaining.saturating_sub(padding);
        if 0 < desc_budget {
            description = truncate_to_width(&candidate.description, desc_budget);
        }
    }

    MenuLineParts {
        label,
        padding,
        description,
    }
}

fn render_menu_block_with_max_rows(
    view: &CompletionView,
    theme: &Theme,
    terminal_width: usize,
    max_rows: usize,
) -> StyledBlock {
    let selected_style = resolve::resolve(theme, tau_themes::names::COMPLETION_SELECTED);
    let label_style = resolve::resolve(theme, tau_themes::names::COMPLETION_LABEL);
    let desc_style = resolve::resolve(theme, tau_themes::names::COMPLETION_DESC);

    let visible = visible_candidate_range(view, max_rows);
    let max_label_width = view.candidates[visible.clone()]
        .iter()
        .map(|c| display_width(c.label.as_str()))
        .max()
        .unwrap_or(0);

    let mut spans: Vec<Span> = Vec::new();
    for (row, i) in visible.enumerate() {
        let candidate = &view.candidates[i];
        if 0 < row {
            spans.push(Span::plain("\n"));
        }

        let is_selected = view.selected == Some(i);
        let parts = menu_line_parts(candidate, max_label_width, terminal_width);

        let line_text = if terminal_width < 4 {
            truncate_to_width(&parts.label, terminal_width)
        } else if parts.description.is_empty() {
            format!("  {}  ", parts.label)
        } else {
            format!(
                "  {}{:padding$}{}  ",
                parts.label,
                "",
                parts.description,
                padding = parts.padding,
            )
        };

        if terminal_width < 4 {
            spans.push(Span::plain(line_text));
        } else if is_selected {
            spans.push(Span::new(line_text, selected_style));
        } else {
            spans.push(Span::plain("  "));
            spans.push(Span::new(parts.label, label_style));
            if !parts.description.is_empty() {
                spans.push(Span::plain(format!(
                    "{:padding$}",
                    "",
                    padding = parts.padding
                )));
                spans.push(Span::new(parts.description, desc_style));
            }
            spans.push(Span::plain("  "));
        }
    }

    StyledBlock::new(StyledText::from(spans))
}

#[cfg(test)]
mod render_tests;
