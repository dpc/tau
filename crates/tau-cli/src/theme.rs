use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::{fmt, io};

use tau_config::settings::{CliTheme, TauDirs};
use tau_themes::{SpanTree, ThemedText};

const THEME_ENV: &str = "TAU_THEME";
const THEME_CHOICE_METADATA_LIMIT: u64 = 64 * 1024;

pub(crate) fn active_prompt_marker(
    theme: &tau_themes::Theme,
    prompt_symbol: &str,
    role: Option<&str>,
) -> tau_cli_term::StyledText {
    let mut text = ThemedText::new();
    let base_style = text.add_style(tau_themes::names::PROMPT_MARKER);
    let marker = format!("{prompt_symbol} ");

    let marker = if let Some(role) = role {
        let role_style = text.add_style(prompt_marker_role_style(role));
        SpanTree::span(
            base_style,
            vec![SpanTree::span(role_style, vec![SpanTree::text(marker)])],
        )
    } else {
        SpanTree::span(base_style, vec![SpanTree::text(marker)])
    };

    text.push_tree(marker);
    tau_cli_term::resolve::themed_text(theme, &text)
}

pub(crate) fn prompt_input_placeholder(
    theme: &tau_themes::Theme,
    role: Option<&str>,
    current_agent_id: Option<&str>,
    current_agent_suspended: bool,
) -> tau_cli_term::StyledText {
    let role = role.unwrap_or("agent");
    let mut text = ThemedText::new();
    let placeholder_style = text.add_style(tau_themes::names::PROMPT_PLACEHOLDER);
    let role_style = text.add_style(tau_themes::names::STATUS_ROLE);

    let parts = if current_agent_suspended {
        vec![SpanTree::text(
            "This agent is suspended. Use /resume before sending messages.",
        )]
    } else {
        match current_agent_id {
            Some(agent_id) => vec![
                SpanTree::text("Write a message to "),
                SpanTree::span(role_style, vec![SpanTree::text(agent_id)]),
                SpanTree::text("..."),
            ],
            None => vec![
                SpanTree::text("Write a message to start a new "),
                SpanTree::span(role_style, vec![SpanTree::text(role)]),
                SpanTree::text(" agent..."),
            ],
        }
    };

    text.push_tree(SpanTree::span(placeholder_style, parts));
    tau_cli_term::resolve::themed_text(theme, &text)
}

pub(crate) fn cwd_right_prompt(
    theme: &tau_themes::Theme,
    cwd: &Path,
    home: Option<&Path>,
) -> tau_cli_term::StyledText {
    let mut text = ThemedText::new();
    let cwd_style = text.add_style(tau_themes::names::PROMPT_CWD);
    text.push(cwd_style, display_cwd(cwd, home));
    tau_cli_term::resolve::themed_text(theme, &text)
}

fn display_cwd(cwd: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty())
        && let Ok(relative) = cwd.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", relative.display());
    }

    cwd.display().to_string()
}

fn prompt_marker_role_style(role: &str) -> String {
    format!("{}.{}", tau_themes::names::PROMPT_MARKER, role)
}

pub(crate) fn select_theme(
    dirs: &TauDirs,
    mode: CliTheme,
) -> Result<tau_themes::Theme, ThemeError> {
    select_theme_with_env_override(dirs, mode, env_theme_override())
}

fn select_theme_with_env_override(
    dirs: &TauDirs,
    mode: CliTheme,
    env_override: Option<CliTheme>,
) -> Result<tau_themes::Theme, ThemeError> {
    let mode = env_override.unwrap_or(mode);
    select_theme_without_env(dirs, mode)
}

/// Selects a theme for an explicit runtime UI command, ignoring `TAU_THEME`.
pub(crate) fn select_theme_for_command(
    dirs: &TauDirs,
    name: &str,
) -> Result<tau_themes::Theme, ThemeError> {
    let Some(mode) = parse_theme_name(name) else {
        return Err(ThemeError::InvalidName(name.to_owned()));
    };
    select_theme_without_env(dirs, mode)
}

fn select_theme_without_env(
    dirs: &TauDirs,
    mode: CliTheme,
) -> Result<tau_themes::Theme, ThemeError> {
    match mode {
        CliTheme::Named(name) => select_named_theme(dirs, &name),
    }
}

fn select_named_theme(dirs: &TauDirs, name: &str) -> Result<tau_themes::Theme, ThemeError> {
    if let Some(theme) = tau_themes::Theme::builtin_named(name) {
        return Ok(theme);
    }

    validate_external_theme_name(name)?;
    let Some(config_dir) = &dirs.config_dir else {
        return Err(ThemeError::NoConfigDir {
            name: name.to_owned(),
        });
    };
    let path = external_theme_path(config_dir, name);
    tau_themes::Theme::load(&path).map_err(|source| ThemeError::Load { path, source })
}

/// One theme shown to `/theme` argument completion and listing output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThemeChoice {
    /// Theme selector typed after `/theme`.
    pub(crate) name: String,
    /// Human-facing description for completion menus and no-argument listings.
    pub(crate) description: String,
}

impl ThemeChoice {
    /// Formats this theme choice for no-argument `/theme` output.
    pub(crate) fn into_listing_text(self) -> String {
        if self.description.is_empty() {
            self.name
        } else {
            format!("{} — {}", self.name, self.description)
        }
    }
}

/// Lists built-in and user-defined theme files available to this UI.
pub(crate) fn available_theme_choices(dirs: &TauDirs) -> Vec<ThemeChoice> {
    let mut choices = BTreeMap::new();
    for name in tau_themes::theme::BUILTIN_THEME_NAMES {
        let theme =
            tau_themes::Theme::builtin_named(name).expect("built-in theme registry name resolves");
        let description = theme.description().unwrap_or_default().to_owned();
        choices.insert((*name).to_owned(), description);
    }
    for choice in external_theme_choices(dirs) {
        choices.entry(choice.name).or_insert(choice.description);
    }
    choices
        .into_iter()
        .map(|(name, description)| ThemeChoice { name, description })
        .collect()
}

fn external_theme_choices(dirs: &TauDirs) -> Vec<ThemeChoice> {
    let Some(config_dir) = &dirs.config_dir else {
        return Vec::new();
    };
    let themes_dir = config_dir.join("themes");
    let entries = match std::fs::read_dir(&themes_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(target: "tau_cli::theme", path = %themes_dir.display(), %error, "failed to enumerate theme completions");
            return Vec::new();
        }
    };
    let builtin_names: BTreeSet<String> = tau_themes::theme::BUILTIN_THEME_NAMES
        .iter()
        .copied()
        .map(str::to_ascii_lowercase)
        .collect();
    let mut choices = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json5")) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let normalized_stem = stem.to_ascii_lowercase();
        if builtin_names.contains(&normalized_stem) || validate_external_theme_name(stem).is_err() {
            continue;
        }
        let description = match entry.file_type() {
            Ok(file_type) if file_type.is_file() => read_theme_choice_description(&path),
            _ => String::new(),
        };
        choices.push(ThemeChoice {
            name: stem.to_owned(),
            description,
        });
    }
    choices.sort_by(|left, right| left.name.cmp(&right.name));
    choices.dedup_by(|left, right| left.name == right.name);
    choices
}

fn read_theme_choice_description(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut limited = file.take(THEME_CHOICE_METADATA_LIMIT + 1);
    let mut contents = String::new();
    if limited.read_to_string(&mut contents).is_err() {
        return String::new();
    }
    if contents.len() as u64 > THEME_CHOICE_METADATA_LIMIT {
        return String::new();
    }
    tau_themes::Theme::parse(&contents)
        .ok()
        .and_then(|theme| theme.description().map(str::to_owned))
        .unwrap_or_default()
}

fn env_theme_override() -> Option<CliTheme> {
    let value = std::env::var(THEME_ENV).ok()?;
    parse_theme_name(&value)
}

fn parse_theme_name(value: &str) -> Option<CliTheme> {
    CliTheme::parse_name(value)
}

fn external_theme_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join("themes").join(format!("{name}.json5"))
}

fn validate_external_theme_name(name: &str) -> Result<(), ThemeError> {
    let path = Path::new(name);
    if name.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(ThemeError::InvalidName(name.to_owned()));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum ThemeError {
    InvalidName(String),
    NoConfigDir {
        name: String,
    },
    Load {
        path: PathBuf,
        source: tau_themes::theme::ThemeLoadError,
    },
}

fn builtin_theme_list() -> String {
    tau_themes::theme::BUILTIN_THEME_NAMES.join(", ")
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid theme name `{name}`: external themes must be named by a single path component"
            ),
            Self::NoConfigDir { name } => write!(
                f,
                "theme `{name}` is not a built-in theme and no Tau config directory is available"
            ),
            Self::Load { path, source } => {
                if matches!(source, tau_themes::theme::ThemeLoadError::Io(_, err) if err.kind() == io::ErrorKind::NotFound)
                {
                    write!(
                        f,
                        "theme file not found: {} (built-ins: {})",
                        path.display(),
                        builtin_theme_list()
                    )
                } else {
                    write!(f, "failed to load theme from {}: {source}", path.display())
                }
            }
        }
    }
}

impl std::error::Error for ThemeError {}

#[cfg(test)]
mod tests;
