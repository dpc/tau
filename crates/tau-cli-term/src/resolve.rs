//! Bridge between [`tau_themes`] and [`tau_cli_term_raw`] types.
//!
//! Converts theme styles into terminal-renderable styles.

use tau_cli_term_raw::{Color, Span, Style, StyledBlock, StyledText};
use tau_themes::{ResolvedSpan, StyleName, Theme, ThemeStyle, ThemedText};

/// Resolves a style name through a theme into a terminal [`Style`].
pub fn resolve(theme: &Theme, name: &str) -> Style {
    convert_style(theme.resolve_style(&StyleName::new(name)))
}

/// Converts a [`ThemeStyle`] into a terminal [`Style`].
pub fn convert_style(ts: ThemeStyle) -> Style {
    Style {
        fg: ts.fg.map(convert_color),
        bg: ts.bg.map(convert_color),
        bold: ts.bold,
        underline: ts.underline,
        italic: ts.italic,
        strikethrough: ts.strikethrough,
    }
}

/// Resolves themed text into terminal styled text.
pub fn themed_text(theme: &Theme, text: &ThemedText) -> StyledText {
    StyledText::from(
        theme
            .resolve(text)
            .into_iter()
            .map(resolved_span)
            .collect::<Vec<_>>(),
    )
}

fn resolved_span(span: ResolvedSpan<'_>) -> Span {
    Span::new(span.text.to_owned(), convert_style(span.style))
}

/// Creates a [`StyledBlock`] using a theme style.
///
/// The style's foreground/bold/etc. apply to the text span; the
/// style's background fills the full block width.
pub fn themed_block(theme: &Theme, name: &str, text: impl Into<String>) -> StyledBlock {
    let mut themed = ThemedText::new();
    let style = themed.add_style(name);
    themed.push(style, text);

    let ts = theme.resolve_style(&StyleName::new(name));
    let mut block = StyledBlock::new(themed_text(theme, &themed));
    if let Some(bg) = ts.bg {
        block = block.bg(convert_color(bg));
    }
    block
}

/// Converts a theme [`tau_themes::Color`] to a terminal
/// `tau_cli_term_raw::Color`.
pub fn convert_color(c: tau_themes::Color) -> Color {
    use tau_themes::Color as TC;
    match c {
        TC::Black => Color::Black,
        TC::DarkRed => Color::DarkRed,
        TC::DarkGreen => Color::DarkGreen,
        TC::DarkYellow => Color::DarkYellow,
        TC::DarkBlue => Color::DarkBlue,
        TC::DarkMagenta => Color::DarkMagenta,
        TC::DarkCyan => Color::DarkCyan,
        TC::DarkGrey => Color::DarkGrey,
        TC::Red => Color::Red,
        TC::Green => Color::Green,
        TC::Yellow => Color::Yellow,
        TC::Blue => Color::Blue,
        TC::Magenta => Color::Magenta,
        TC::Cyan => Color::Cyan,
        TC::White => Color::White,
        TC::Grey => Color::Grey,
        TC::Rgb { r, g, b } => Color::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests;
