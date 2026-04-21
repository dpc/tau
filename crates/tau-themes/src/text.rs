//! Themed text representation.
//!
//! [`ThemedText`] pairs style *names* with text spans. The actual
//! visual attributes are resolved later via a [`Theme`](crate::Theme).

use std::fmt;

/// A semantic style name (e.g. `"prompt"`, `"error"`, `"muted"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(transparent)]
pub struct StyleName(String);

impl StyleName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StyleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for StyleName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for StyleName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Index into [`ThemedText::styles`]. Values beyond the styles
/// array (including [`StyleIdx::DEFAULT`]) resolve to the default
/// (no formatting) style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleIdx(u16);

impl StyleIdx {
    /// Sentinel value that always resolves to the default style.
    pub const DEFAULT: Self = Self(u16::MAX);

    pub fn raw(self) -> u16 {
        self.0
    }
}

/// A span of text tagged with a style index.
#[derive(Clone, Debug)]
pub struct ThemedSpan {
    pub style_idx: StyleIdx,
    pub text: String,
}

/// Themed text: a list of style names plus spans that reference them
/// by index.
///
/// The indirection (`StyleIdx` → `StyleName`) avoids repeating
/// style-name strings in every span.
#[derive(Clone, Debug, Default)]
pub struct ThemedText {
    styles: Vec<StyleName>,
    spans: Vec<ThemedSpan>,
}

impl ThemedText {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a style name and returns its index.
    ///
    /// Duplicate names are allowed — each call allocates a new slot.
    pub fn add_style(&mut self, name: impl Into<StyleName>) -> StyleIdx {
        let idx = self.styles.len();
        self.styles.push(name.into());
        // Truncate to u16; styles beyond u16::MAX - 1 are
        // unreachable since DEFAULT is u16::MAX.
        StyleIdx(idx as u16)
    }

    /// Appends a span with the given style index.
    pub fn push(&mut self, idx: StyleIdx, text: impl Into<String>) {
        self.spans.push(ThemedSpan {
            style_idx: idx,
            text: text.into(),
        });
    }

    /// Appends a span with the default (no formatting) style.
    pub fn push_default(&mut self, text: impl Into<String>) {
        self.push(StyleIdx::DEFAULT, text);
    }

    /// Returns the registered style names.
    pub fn styles(&self) -> &[StyleName] {
        &self.styles
    }

    /// Returns the spans.
    pub fn spans(&self) -> &[ThemedSpan] {
        &self.spans
    }

    /// Looks up the [`StyleName`] for a span's index, or `None` if
    /// the index is out of bounds (default style).
    pub fn style_name(&self, idx: StyleIdx) -> Option<&StyleName> {
        self.styles.get(idx.0 as usize)
    }
}
