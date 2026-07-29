//! Theme definition and resolution.
//!
//! A [`Theme`] carries optional human-facing metadata and maps [`StyleName`]s
//! to [`ThemeStyle`]s. Resolution takes a [`ThemedText`] and produces
//! [`ResolvedSpan`]s with concrete style attributes.

use std::collections::HashMap;
use std::path::Path;
use std::{fmt, io};

use crate::color::Color;
use crate::text::{SpanTree, StyleIdx, StyleName, ThemedText};

/// Visual attributes for a style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeStyle {
    /// Optional foreground/text color.
    pub fg: Option<Color>,
    /// Optional background color.
    pub bg: Option<Color>,
    /// Whether text should be bold.
    pub bold: bool,
    /// Whether text should be underlined.
    pub underline: bool,
    /// Whether text should be italicized.
    pub italic: bool,
    /// Whether text should be struck through.
    pub strikethrough: bool,
}

impl ThemeStyle {
    fn override_with(self, inner: Self) -> Self {
        Self {
            fg: inner.fg.or(self.fg),
            bg: inner.bg.or(self.bg),
            bold: self.bold || inner.bold,
            underline: self.underline || inner.underline,
            italic: self.italic || inner.italic,
            strikethrough: self.strikethrough || inner.strikethrough,
        }
    }
}

/// A theme: optional user-facing metadata plus a mapping from style names to
/// visual attributes.
///
/// Styles not present in the map resolve to [`ThemeStyle::default()`]
/// (no formatting).
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// Human-facing summary shown when listing or completing available themes.
    description: Option<String>,
    #[serde(default)]
    styles: HashMap<StyleName, ThemeStyle>,
}

const BUILTIN_DEFAULT_THEME: &str = include_str!("../themes/tau-plain-dark.json5");
const BUILTIN_DPC_THEME: &str = include_str!("../themes/tau-dpc.json5");
const BUILTIN_LIGHT_THEME: &str = include_str!("../themes/tau-plain-light.json5");

/// User-selectable theme names embedded in Tau.
pub const BUILTIN_THEME_NAMES: &[&str] = &["tau-plain-dark", "tau-plain-light", "tau-dpc"];

impl Theme {
    /// Creates an empty theme (everything uses default styling).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the built-in "tau-plain-dark" theme.
    ///
    /// This is the default theme used when no user theme is configured.
    pub fn builtin() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_DEFAULT_THEME).expect("built-in tau-plain-dark theme is valid JSON5")
    }

    /// Returns the built-in "tau-dpc" theme.
    pub fn builtin_dpc() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_DPC_THEME).expect("built-in tau-dpc theme is valid JSON5")
    }

    /// Returns the built-in "tau-plain-light" theme.
    pub fn builtin_light() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_LIGHT_THEME).expect("built-in tau-plain-light theme is valid JSON5")
    }

    /// Returns the built-in theme selected by `name`, case-insensitively.
    #[must_use]
    pub fn builtin_named(name: &str) -> Option<Self> {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: stringly-typed-match
        Some(match name.to_ascii_lowercase().as_str() {
            "tau-plain-dark" => Self::builtin(),
            "tau-dpc" => Self::builtin_dpc(),
            "tau-plain-light" => Self::builtin_light(),
            _ => return None,
        })
    }

    /// Loads a theme from a JSON5 file.
    pub fn load(path: &Path) -> Result<Self, ThemeLoadError> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| ThemeLoadError::Io(path.to_path_buf(), e))?;
        Self::parse(&contents).map_err(|e| ThemeLoadError::Parse(path.to_path_buf(), e))
    }

    /// Parses a theme from a JSON5 string.
    pub fn parse(s: &str) -> Result<Self, json5::Error> {
        json5::from_str(s)
    }

    /// Returns the human-facing theme description, if the theme file provides
    /// one.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Looks up the style for a name, falling back to the default.
    pub fn resolve_style(&self, name: &StyleName) -> ThemeStyle {
        self.styles.get(name).copied().unwrap_or_default()
    }

    /// Resolves a [`ThemedText`] into spans with concrete styles.
    pub fn resolve<'a>(&self, themed: &'a ThemedText) -> Vec<ResolvedSpan<'a>> {
        let mut out = Vec::new();
        let mut stack = Vec::new();
        self.resolve_tree(themed, themed.spans(), &mut stack, &mut out);
        out
    }

    fn resolve_tree<'a>(
        &self,
        themed: &'a ThemedText,
        span: &'a SpanTree<StyleIdx>,
        stack: &mut Vec<ThemeStyle>,
        out: &mut Vec<ResolvedSpan<'a>>,
    ) {
        match span {
            SpanTree::Text(text) => out.push(ResolvedSpan {
                text,
                style: effective_style(stack),
            }),
            SpanTree::Span { style, text } => {
                stack.push(
                    themed
                        .style_name(*style)
                        .map(|name| self.resolve_style(name))
                        .unwrap_or_default(),
                );
                for child in text {
                    self.resolve_tree(themed, child, stack, out);
                }
                stack.pop();
            }
        }
    }
}

fn effective_style(stack: &[ThemeStyle]) -> ThemeStyle {
    stack
        .iter()
        .fold(ThemeStyle::default(), |effective, style| {
            effective.override_with(*style)
        })
}

/// A span of text with resolved style attributes.
pub struct ResolvedSpan<'a> {
    /// Text fragment covered by this resolved span.
    pub text: &'a str,
    /// Effective style attributes for [`Self::text`].
    pub style: ThemeStyle,
}

/// Errors that can occur when loading a theme file.
#[derive(Debug)]
pub enum ThemeLoadError {
    /// Reading the theme file failed.
    Io(std::path::PathBuf, io::Error),
    /// Parsing the theme file as JSON5 failed.
    Parse(std::path::PathBuf, json5::Error),
}

impl fmt::Display for ThemeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "reading {}: {err}", path.display()),
            Self::Parse(path, err) => write!(f, "parsing {}: {err}", path.display()),
        }
    }
}

impl std::error::Error for ThemeLoadError {}

#[cfg(test)]
mod tests;
