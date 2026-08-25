use super::*;

/// Built-in themes must present typed internal notices consistently even when
/// their color palettes differ.
#[test]
fn builtins_italicize_internal_notices() {
    let name = crate::StyleName::new(crate::names::SYSTEM_INTERNAL_NOTICE);
    for builtin_name in BUILTIN_THEME_NAMES {
        let theme = Theme::builtin_named(builtin_name)
            .unwrap_or_else(|| panic!("registered built-in theme `{builtin_name}` must resolve"));
        assert!(theme.resolve_style(&name).italic);
    }
}

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

/// Ensures a registered semantic style resolves every field while partial and
/// empty definitions retain the default values for omitted fields.
#[test]
fn named_style_resolves() {
    let theme: Theme = Theme::parse(
        r#"{
                styles: {
                    prompt: {
                        fg: "green",
                        bg: "dark_blue",
                        bold: true,
                        underline: true,
                        italic: true,
                        strikethrough: true,
                    },
                    partial: { fg: "red" },
                    empty: {},
                }
            }"#,
    )
    .expect("valid theme");

    let mut text = ThemedText::new();
    let prompt = text.add_style("prompt");
    text.push(prompt, ">");

    let resolved = theme.resolve(&text);
    assert_eq!(
        resolved[0].style,
        ThemeStyle {
            fg: Some(Color::Green),
            bg: Some(Color::DarkBlue),
            bold: true,
            underline: true,
            italic: true,
            strikethrough: true,
        }
    );
    assert_eq!(
        theme.resolve_style(&StyleName::new("partial")),
        ThemeStyle {
            fg: Some(Color::Red),
            ..ThemeStyle::default()
        }
    );
    assert_eq!(
        theme.resolve_style(&StyleName::new("empty")),
        ThemeStyle::default()
    );
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

/// Ensures the default built-in theme resolves submitted user prompts to bright
/// white, keeping them distinct from terminal-default assistant text.
#[test]
fn builtin_default_theme_resolves_submitted_user_prompts_as_bright_white() {
    let theme = Theme::builtin();

    let prompt = theme.resolve_style(&StyleName::new("user.prompt"));
    assert_eq!(prompt.fg, Some(Color::White));
}

/// Ensures the built-in theme registry stays synchronized with name lookup and
/// does not accidentally keep removed legacy aliases selectable as built-ins.
#[test]
fn builtin_theme_names_match_lookup_registry() {
    let canonical = ["tau-plain-dark", "tau-plain-light", "tau-dpc"];
    assert_eq!(BUILTIN_THEME_NAMES, canonical);

    for name in canonical {
        assert!(
            Theme::builtin_named(name).is_some(),
            "{name} should resolve"
        );
        assert!(
            Theme::builtin_named(&name.to_ascii_uppercase()).is_some(),
            "{name} should resolve case-insensitively"
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

/// Ensures every built-in theme keeps Markdown links visibly distinct.
#[test]
fn builtin_themes_style_markdown_links_bold_red() {
    for name in BUILTIN_THEME_NAMES {
        let theme = Theme::builtin_named(name).expect("registered built-in theme");
        let style = theme.resolve_style(&StyleName::new(crate::names::MARKDOWN_LINK));
        assert!(style.bold, "{name} Markdown links must be bold");
        assert_eq!(
            style.fg,
            Some(Color::Red),
            "{name} Markdown links must be red"
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
                    | Some(Color::White)
            ),
            "{name} sets an unsafe foreground color: {:?}",
            style.fg
        );
        assert!(style.bg.is_none(), "{name} sets a background color");
    }
}

/// Ensures the personalized `tau-dpc` theme explicitly renders submitted user
/// prompts bright white instead of inheriting the terminal-default foreground.
#[test]
fn builtin_dpc_theme_resolves_submitted_user_prompts_as_bright_white() {
    let theme = Theme::builtin_dpc();
    let prompt = theme.resolve_style(&StyleName::new("user.prompt"));

    assert!(prompt.bold);
    assert_eq!(prompt.fg, Some(Color::White));
    assert!(prompt.bg.is_none());
}

/// Ensures user-authored top-level and style-field typos are rejected instead
/// of being silently ignored and producing confusing styling behavior.
#[test]
fn theme_rejects_unknown_fields() {
    for input in [
        r#"{ unexpected: true }"#,
        r#"{ styles: { prompt: { foreground: "green" } } }"#,
    ] {
        let error = Theme::parse(input).expect_err("unknown field should fail");
        assert!(error.to_string().contains("unknown field"), "got: {error}");
    }
}

/// Ensures every built-in emphasizes under-quota status and distinguishes
/// dangerous quota status from over-limit and unknown status.
#[test]
fn builtins_emphasize_under_quota_and_distinguish_danger() {
    for name in BUILTIN_THEME_NAMES {
        let theme = Theme::builtin_named(name).expect("registered built-in theme");
        let under = theme.resolve_style(&StyleName::new(crate::names::STATUS_QUOTA_UNDER));
        let aligned = theme.resolve_style(&StyleName::new(crate::names::STATUS_QUOTA_ALIGNED));
        let over = theme.resolve_style(&StyleName::new(crate::names::STATUS_QUOTA_OVER));
        let danger = theme.resolve_style(&StyleName::new(crate::names::STATUS_QUOTA_DANGER));
        let unknown = theme.resolve_style(&StyleName::new(crate::names::STATUS_QUOTA_UNKNOWN));
        assert!(under.bold);
        assert!(under.fg.is_some());
        assert!(aligned.fg.is_some());
        assert!(over.fg.is_some());
        assert!(danger.fg.is_some());
        assert_ne!(over.fg, danger.fg);
        assert_ne!(unknown.fg, danger.fg);
    }
}
