use super::*;

/// Ensures an empty theme is safe to use and leaves all text unstyled rather
/// than requiring callers to special-case missing theme configuration.
#[test]
fn empty_theme_resolves_to_defaults() {
    let theme = Theme::new();
    let mut text = ThemedText::new();
    let s = text.add_style("whatever");
    text.push(s, "hello");

    let resolved = theme.resolve(&text);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].text, "hello");
    assert_eq!(resolved[0].style, ThemeStyle::default());
}

/// Ensures a registered semantic style name resolves through the theme's style
/// table and carries all supported attributes.
#[test]
fn named_style_resolves() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    prompt: { fg: "green", bold: true, strikethrough: true },
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let prompt = text.add_style("prompt");
    text.push(prompt, ">");

    let resolved = theme.resolve(&text);
    assert_eq!(resolved[0].style.fg, Some(Color::Green));
    assert!(resolved[0].style.bold);
    assert!(!resolved[0].style.italic);
    assert!(resolved[0].style.strikethrough);
}

/// Ensures theme files may carry user-facing metadata without changing style
/// resolution, and that old files which omit the metadata remain valid.
#[test]
fn description_metadata_is_optional_and_parsed() {
    let described = Theme::parse(
        r#"{
                description: "Readable theme description",
                styles: {
                    prompt: { fg: "green" },
                }
            }"#,
    )
    .expect("valid described theme");
    let old_format = Theme::parse(r#"{ styles: { prompt: { fg: "green" } } }"#)
        .expect("valid undescribed theme");

    assert_eq!(described.description(), Some("Readable theme description"));
    assert_eq!(old_format.description(), None);
}

/// Ensures spans explicitly marked with the default index bypass any configured
/// theme styles and resolve to no formatting.
#[test]
fn default_idx_resolves_to_default_style() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    prompt: { fg: "red" },
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    text.push_default("plain text");

    let resolved = theme.resolve(&text);
    assert_eq!(resolved[0].style, ThemeStyle::default());
}

/// Ensures JSON5 themes can use web-style hex RGB colors for exact theme
/// tuning, not only the named ANSI color palette.
#[test]
fn hex_color_in_theme() {
    let theme: Theme = Theme::parse(
        r##"{
                styles: {
                    custom: { fg: "#ff8800", bg: "#001122" },
                }
            }"##,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let s = text.add_style("custom");
    text.push(s, "colored");

    let resolved = theme.resolve(&text);
    assert_eq!(
        resolved[0].style.fg,
        Some(Color::Rgb {
            r: 0xff,
            g: 0x88,
            b: 0x00
        })
    );
    assert_eq!(
        resolved[0].style.bg,
        Some(Color::Rgb {
            r: 0x00,
            g: 0x11,
            b: 0x22
        })
    );
}

/// Ensures sibling spans do not leak styles into each other and default spans
/// remain unstyled after styled siblings.
#[test]
fn multiple_spans_resolve_independently() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    error: { fg: "red", bold: true },
                    muted: { fg: "dark_grey" },
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let error = text.add_style("error");
    let muted = text.add_style("muted");
    text.push(error, "ERROR: ");
    text.push(muted, "details here");
    text.push_default(" (ok)");

    let resolved = theme.resolve(&text);
    assert_eq!(resolved.len(), 3);

    assert_eq!(resolved[0].style.fg, Some(Color::Red));
    assert!(resolved[0].style.bold);

    assert_eq!(resolved[1].style.fg, Some(Color::DarkGrey));
    assert!(!resolved[1].style.bold);

    assert_eq!(resolved[2].style, ThemeStyle::default());
}

/// Ensures nested spans inherit unspecified attributes from outer spans while
/// allowing inner spans to override attributes they set.
#[test]
fn nested_spans_inherit_and_override_styles() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    outer: { fg: "red", bg: "dark_blue", bold: true },
                    inner: { fg: "green", italic: true },
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let outer = text.add_style("outer");
    let inner = text.add_style("inner");
    text.push_tree(SpanTree::span(
        outer,
        vec![
            SpanTree::text("outer "),
            SpanTree::span(inner, vec![SpanTree::text("inner")]),
        ],
    ));

    let resolved = theme.resolve(&text);
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].text, "outer ");
    assert_eq!(resolved[0].style.fg, Some(Color::Red));
    assert_eq!(resolved[0].style.bg, Some(Color::DarkBlue));
    assert!(resolved[0].style.bold);
    assert!(!resolved[0].style.italic);

    assert_eq!(resolved[1].text, "inner");
    assert_eq!(resolved[1].style.fg, Some(Color::Green));
    assert_eq!(resolved[1].style.bg, Some(Color::DarkBlue));
    assert!(resolved[1].style.bold);
    assert!(resolved[1].style.italic);
}

/// Ensures the default built-in theme parses and keeps representative colors
/// inside its intentionally small safe-color set.
#[test]
fn builtin_default_theme_parses_and_uses_safe_colors() {
    let theme = Theme::builtin();

    let prompt = theme.resolve_style(&StyleName::new("user.prompt"));
    assert!(prompt.bold);
    assert!(prompt.fg.is_none());
    assert!(prompt.bg.is_none());

    let tool_err = theme.resolve_style(&StyleName::new("tool.status.error"));
    assert_eq!(tool_err.fg, Some(Color::Red));
    assert!(tool_err.bg.is_none());

    let tool_name = theme.resolve_style(&StyleName::new("tool.name"));
    assert_eq!(tool_name.fg, Some(Color::Yellow));
    assert!(tool_name.bg.is_none());

    let watching_name = theme.resolve_style(&StyleName::new(crate::names::WATCHING_NAME));
    assert_eq!(watching_name.fg, Some(Color::DarkYellow));
    assert!(watching_name.bg.is_none());

    let progress = theme.resolve_style(&StyleName::new(crate::names::PROGRESS_INDICATOR));
    assert_eq!(progress.fg, Some(Color::Cyan));
    assert!(progress.bold);
    assert!(progress.bg.is_none());

    let selected = theme.resolve_style(&StyleName::new("completion.selected"));
    assert!(selected.bold);
    assert!(selected.underline);
    assert!(selected.fg.is_none());
    assert!(selected.bg.is_none());
}

/// Ensures the built-in theme registry stays synchronized with name lookup and
/// does not accidentally keep removed legacy aliases selectable as built-ins.
#[test]
fn builtin_theme_names_match_lookup_registry() {
    for name in BUILTIN_THEME_NAMES {
        assert!(
            Theme::builtin_named(name).is_some(),
            "{name} should resolve"
        );
    }
    for removed_alias in ["default", "dpc", "tau-light", "auto", "dark", "light"] {
        assert!(
            Theme::builtin_named(removed_alias).is_none(),
            "{removed_alias} should not resolve as a built-in"
        );
    }
}

/// Ensures every built-in theme can distinguish passive watched-agent labels
/// from active tool-call labels.
#[test]
fn builtin_watching_name_differs_from_tool_name() {
    for name in BUILTIN_THEME_NAMES {
        let theme = Theme::builtin_named(name).expect("built-in theme");
        let tool_name = theme.resolve_style(&StyleName::new(crate::names::TOOL_NAME));
        let watching_name = theme.resolve_style(&StyleName::new(crate::names::WATCHING_NAME));
        assert_ne!(
            watching_name.fg, tool_name.fg,
            "{name} should use a different foreground color for watching.name than tool.name"
        );
        assert_ne!(
            watching_name, tool_name,
            "{name} should render watching.name differently from tool.name"
        );
    }
}

/// Ensures every explicitly configured style in the conservative default theme
/// stays within the allowed foreground color set and does not set backgrounds.
#[test]
fn builtin_default_theme_styles_stay_palette_safe() {
    let theme = Theme::builtin();

    for (name, style) in &theme.styles {
        assert!(
            matches!(
                style.fg,
                None | Some(Color::Yellow)
                    | Some(Color::DarkYellow)
                    | Some(Color::Cyan)
                    | Some(Color::Green)
                    | Some(Color::Red)
            ),
            "{name} sets an unsafe foreground color: {:?}",
            style.fg
        );
        assert!(style.bg.is_none(), "{name} sets a background color");
    }
}

/// Ensures the personalized `tau-dpc` built-in theme parses without
/// snapshotting any visual choices, so future theme tuning does not churn test
/// expectations.
#[test]
fn builtin_dpc_theme_parses() {
    let theme = Theme::builtin_dpc();
    let _ = theme.resolve_style(&StyleName::new("user.prompt"));
}

/// Ensures user-authored theme typos are rejected instead of being silently
/// ignored and producing confusing styling behavior.
#[test]
fn theme_rejects_unknown_fields() {
    // Theme files are user-authored config. Unknown top-level or style fields
    // should fail fast instead of silently ignoring misspelled keys.
    let error = Theme::parse(
        r#"{
                styles: {
                    prompt: { foreground: "green" },
                }
            }"#,
    )
    .expect_err("unknown style field should fail");

    assert!(error.to_string().contains("unknown field"), "got: {error}");
}

/// Ensures the light built-in theme parses without snapshotting its visual
/// choices, which are intentionally independent from renderer behavior tests.
#[test]
fn builtin_light_theme_parses() {
    let theme = Theme::builtin_light();
    let _ = theme.resolve_style(&StyleName::new("user.prompt"));
}

/// Ensures callers can safely resolve missing style names in built-in themes
/// without receiving stale or inherited formatting.
#[test]
fn builtin_theme_missing_style_is_default() {
    let theme = Theme::builtin();
    let style = theme.resolve_style(&StyleName::new("nonexistent.style"));
    assert_eq!(style, ThemeStyle::default());
}
