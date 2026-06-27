//! Themed text representation.
//!
//! [`ThemedText`] pairs style *names* with a tree of text spans. The
//! actual visual attributes are resolved later via a [`Theme`](crate::Theme).

use std::fmt;

/// A semantic style name (e.g. `"prompt"`, `"error"`, `"muted"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(transparent)]
pub struct StyleName(String);

impl StyleName {
    /// Creates a style name from owned or borrowed string-like input.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns the style name as a string slice.
    #[must_use]
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
pub struct StyleIdx(usize);

impl StyleIdx {
    /// Sentinel value that always resolves to the default style.
    pub const DEFAULT: Self = Self(usize::MAX);

    /// Returns the underlying style table index.
    ///
    /// For [`StyleIdx::DEFAULT`], this returns the sentinel value rather than a
    /// valid registered style slot.
    #[must_use]
    pub fn raw(self) -> usize {
        self.0
    }
}

/// A tree of text and styled child spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanTree<S> {
    /// A leaf text fragment.
    Text(String),
    /// A styled node containing child text or span nodes.
    Span {
        /// The style to apply while resolving this span's children.
        style: S,
        /// Child text or span nodes in display order.
        text: Vec<Self>,
    },
}

impl<S> SpanTree<S> {
    /// Creates a text leaf.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Creates a styled span node with the provided children.
    #[must_use]
    pub fn span(style: S, text: Vec<Self>) -> Self {
        Self::Span { style, text }
    }
}

/// Backwards-compatible alias for a styled tree node.
pub type ThemedSpan = SpanTree<StyleIdx>;

/// Themed text: a list of style names plus spans that reference them
/// by index.
///
/// The indirection (`StyleIdx` → `StyleName`) avoids repeating
/// style-name strings in every span. The spans form a tree so inner
/// styles can refine outer styles.
#[derive(Clone, Debug)]
pub struct ThemedText {
    styles: Vec<StyleName>,
    spans: SpanTree<StyleIdx>,
}

impl Default for ThemedText {
    fn default() -> Self {
        Self {
            styles: Vec::new(),
            spans: SpanTree::Span {
                style: StyleIdx::DEFAULT,
                text: Vec::new(),
            },
        }
    }
}

impl ThemedText {
    /// Creates empty themed text.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates themed text from a span tree without pre-registering any styles.
    #[must_use]
    pub fn from_spans(spans: SpanTree<StyleIdx>) -> Self {
        Self {
            styles: Vec::new(),
            spans,
        }
    }

    /// Registers a style name and returns its index.
    ///
    /// Duplicate names are allowed — each call allocates a new slot.
    ///
    /// # Panics
    ///
    /// Panics if the style table reaches [`StyleIdx::DEFAULT`]'s sentinel slot.
    pub fn add_style(&mut self, name: impl Into<StyleName>) -> StyleIdx {
        let idx = self.styles.len();
        assert!(
            idx != StyleIdx::DEFAULT.raw(),
            "too many styles registered in ThemedText"
        );
        self.styles.push(name.into());
        StyleIdx(idx)
    }

    /// Appends a span with the given style index at the root level.
    pub fn push(&mut self, idx: StyleIdx, text: impl Into<String>) {
        self.push_tree(SpanTree::Span {
            style: idx,
            text: vec![SpanTree::Text(text.into())],
        });
    }

    /// Appends a tree at the root level.
    pub fn push_tree(&mut self, span: SpanTree<StyleIdx>) {
        match &mut self.spans {
            SpanTree::Span { text, .. } => text.push(span),
            SpanTree::Text(_) => unreachable!("ThemedText root is always a span"),
        }
    }

    /// Appends a span with the default (no formatting) style.
    pub fn push_default(&mut self, text: impl Into<String>) {
        self.push(StyleIdx::DEFAULT, text);
    }

    /// Returns the registered style names.
    pub fn styles(&self) -> &[StyleName] {
        &self.styles
    }

    /// Returns the span tree.
    pub fn spans(&self) -> &SpanTree<StyleIdx> {
        &self.spans
    }

    /// Looks up the [`StyleName`] for a span's index, or `None` if
    /// the index is out of bounds (default style).
    pub fn style_name(&self, idx: StyleIdx) -> Option<&StyleName> {
        if idx == StyleIdx::DEFAULT {
            return None;
        }
        self.styles.get(idx.0)
    }
}

#[cfg(test)]
mod tests;
