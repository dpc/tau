use super::*;
use crate::{Color, Theme, ThemeStyle};

/// Ensures the explicit default index always means "unstyled", even after the
/// style table grows beyond the old 16-bit sentinel slot.
#[test]
fn default_idx_is_never_a_registered_style() {
    let mut text = ThemedText::new();
    let mut last_style = StyleIdx::DEFAULT;
    for idx in 0..=u16::MAX {
        let style = text.add_style(format!("style.{idx}"));
        assert_ne!(style, StyleIdx::DEFAULT);
        assert_eq!(style.raw(), usize::from(idx));
        last_style = style;
    }

    assert_eq!(text.style_name(StyleIdx::DEFAULT), None);
    assert_eq!(
        text.style_name(last_style).map(StyleName::as_str),
        Some("style.65535")
    );
}

/// Ensures appending to a leaf-root value preserves that leaf until mutation,
/// then wraps it and the supplied tree in order without changing the subtree.
#[test]
fn leaf_root_tree_append_wraps_original_and_preserves_appended_subtree() {
    let original = SpanTree::text("before");
    let appended = SpanTree::span(
        StyleIdx(42),
        vec![
            SpanTree::text("after "),
            SpanTree::span(StyleIdx(7), vec![SpanTree::text("nested")]),
        ],
    );
    let mut text = ThemedText::from_spans(original.clone());

    assert_eq!(text.spans(), &original);

    text.push_tree(appended.clone());

    assert_eq!(
        text.spans(),
        &SpanTree::span(StyleIdx::DEFAULT, vec![original, appended])
    );
}

/// Ensures default appends to a leaf root retain text order and add siblings
/// beneath one wrapper instead of repeatedly wrapping the original text.
#[test]
fn leaf_root_default_appends_share_one_wrapper() {
    let mut text = ThemedText::from_spans(SpanTree::text("before"));

    text.push_default(" middle");
    text.push_default(" after");

    assert_eq!(
        text.spans(),
        &SpanTree::span(
            StyleIdx::DEFAULT,
            vec![
                SpanTree::text("before"),
                SpanTree::span(StyleIdx::DEFAULT, vec![SpanTree::text(" middle")]),
                SpanTree::span(StyleIdx::DEFAULT, vec![SpanTree::text(" after")]),
            ],
        )
    );
}

/// Ensures an empty leaf root remains a distinct first child when an append
/// normalizes the root, preserving empty-text tree structure for callers.
#[test]
fn empty_leaf_root_is_preserved_when_appended() {
    let mut text = ThemedText::from_spans(SpanTree::text(""));

    text.push_default("after");

    assert_eq!(
        text.spans(),
        &SpanTree::span(
            StyleIdx::DEFAULT,
            vec![
                SpanTree::text(""),
                SpanTree::span(StyleIdx::DEFAULT, vec![SpanTree::text("after")]),
            ],
        )
    );
}

/// Ensures a registered style and its index survive leaf-root normalization,
/// leaving the original leaf unstyled while resolving the appended text.
#[test]
fn leaf_root_append_preserves_registered_styles_and_resolution() {
    let mut text = ThemedText::from_spans(SpanTree::text("plain"));
    let accent = text.add_style("accent");

    text.push(accent, "styled");

    assert_eq!(text.styles(), &[StyleName::new("accent")]);
    assert_eq!(
        text.spans(),
        &SpanTree::span(
            StyleIdx::DEFAULT,
            vec![
                SpanTree::text("plain"),
                SpanTree::span(accent, vec![SpanTree::text("styled")]),
            ],
        )
    );

    let theme: Theme = Theme::parse(r#"{ styles: { accent: { fg: "red", bold: true } } }"#)
        .expect("valid accent theme");
    let resolved = theme.resolve(&text);

    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].text, "plain");
    assert_eq!(resolved[0].style, ThemeStyle::default());
    assert_eq!(resolved[1].text, "styled");
    assert_eq!(
        resolved[1].style,
        ThemeStyle {
            fg: Some(Color::Red),
            bold: true,
            ..ThemeStyle::default()
        }
    );
}

/// Ensures appending to an existing span root preserves its style and children,
/// adding only the supplied tree as the final child.
#[test]
fn span_root_append_preserves_existing_root_and_children() {
    let original = SpanTree::span(StyleIdx(3), vec![SpanTree::text("before")]);
    let appended = SpanTree::span(StyleIdx(4), vec![SpanTree::text("after")]);
    let mut text = ThemedText::from_spans(original);

    text.push_tree(appended.clone());

    assert_eq!(
        text.spans(),
        &SpanTree::span(StyleIdx(3), vec![SpanTree::text("before"), appended],)
    );
}
