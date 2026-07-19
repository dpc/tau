use super::*;

/// Ensures CLI and environment theme names share the same normalization path,
/// including external names that must not be rejected during parsing.
#[test]
fn parses_theme_env_values() {
    assert_eq!(
        parse_theme_name(" tau-plain-dark "),
        Some(CliTheme::Named("tau-plain-dark".to_owned()))
    );
    assert_eq!(
        parse_theme_name("solarized"),
        Some(CliTheme::Named("solarized".to_owned()))
    );
    assert_eq!(parse_theme_name("   "), None);
}

/// Ensures built-in theme file names remain selectable without requiring a Tau
/// config directory. This intentionally avoids asserting built-in palette
/// details so theme tuning does not churn selector tests.
#[test]
fn selected_named_builtin_theme() {
    let dirs = tau_config::settings::TauDirs {
        config_dir: None,
        state_dir: None,
    };

    let theme = select_theme(&dirs, CliTheme::Named("tau-dpc".to_owned()))
        .expect("built-in theme loads without config dir");
    let prompt = right_prompt_context(&theme, Path::new("/tmp/project"), None, "session-1");

    assert_eq!(prompt.spans()[0].text, "/tmp/project &session-1");
}

/// Ensures explicitly named built-ins accept documented case-insensitive input.
#[test]
fn selected_named_builtin_theme_case_insensitively() {
    let dirs = tau_config::settings::TauDirs {
        config_dir: None,
        state_dir: None,
    };

    select_theme_for_command(&dirs, "TAU-DPC").expect("case-insensitive built-in loads");
    select_theme_for_command(&dirs, "TAU-PLAIN-LIGHT").expect("case-insensitive built-in loads");
    select_theme_for_command(&dirs, "Tau-Plain-Dark").expect("case-insensitive built-in loads");
}

/// Ensures the runtime `/theme` selection helper honors the user's explicit
/// argument instead of applying the startup-only `TAU_THEME` override.
#[test]
fn command_theme_selection_ignores_env_override() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    std::fs::write(
        themes.join("custom.json5"),
        r##"{ styles: { "prompt.cwd": { fg: "red" } } }"##,
    )
    .expect("write theme");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };
    let startup = select_theme_with_env_override(
        &dirs,
        CliTheme::Named("tau-dpc".to_owned()),
        Some(CliTheme::Named("custom".to_owned())),
    )
    .expect("env theme loads");
    let command = select_theme_for_command(&dirs, "tau-dpc").expect("command theme loads");

    let startup_prompt =
        right_prompt_context(&startup, Path::new("/tmp/project"), None, "session-1");
    let command_prompt =
        right_prompt_context(&command, Path::new("/tmp/project"), None, "session-1");
    assert_eq!(
        startup_prompt.spans()[0].style.fg,
        Some(tau_cli_term::Color::Red)
    );
    assert_ne!(
        startup_prompt.spans()[0].style,
        command_prompt.spans()[0].style
    );
}

/// Ensures external theme names resolve to `themes/<name>.json5` under Tau's
/// config directory and affect normal terminal style resolution.
#[test]
fn selected_external_theme_from_config_themes_dir() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    std::fs::write(
        themes.join("custom.json5"),
        r##"{ styles: { "prompt.cwd": { fg: "red", bold: true } } }"##,
    )
    .expect("write theme");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };

    let theme = select_theme(&dirs, CliTheme::Named("custom".to_owned())).expect("theme loads");
    let prompt = right_prompt_context(&theme, Path::new("/tmp/project"), None, "session-1");

    assert_eq!(prompt.spans()[0].style.fg, Some(tau_cli_term::Color::Red));
    assert!(prompt.spans()[0].style.bold);
}

/// Ensures `/theme` completion/listing can show descriptions for built-in
/// selectors and valid user theme files without exposing path-like or duplicate
/// external names.
#[test]
fn available_theme_choices_include_builtins_and_user_themes() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    std::fs::write(
        themes.join("custom.json5"),
        r#"{ description: "Custom theme from disk", styles: {} }"#,
    )
    .expect("write custom theme");
    std::fs::write(themes.join("tau-dpc.json5"), "{ styles: {} }").expect("write shadowed theme");
    std::fs::write(themes.join("TAU-DPC.json5"), "{ styles: {} }").expect("write shadowed theme");
    std::fs::write(themes.join("not-a-theme.txt"), "ignored").expect("write ignored file");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };

    let choices = available_theme_choices(&dirs);
    let names: Vec<String> = choices.iter().map(|choice| choice.name.clone()).collect();

    for name in tau_themes::theme::BUILTIN_THEME_NAMES
        .iter()
        .copied()
        .chain(["custom"])
    {
        assert!(names.iter().any(|choice| choice == name), "missing {name}");
    }
    assert_eq!(
        names
            .iter()
            .filter(|choice| choice.as_str() == "tau-dpc")
            .count(),
        1
    );
    assert!(!names.iter().any(|choice| choice == "TAU-DPC"));
    for removed_alias in ["auto", "dark", "light", "default", "dpc", "tau-light"] {
        assert!(!names.iter().any(|choice| choice == removed_alias));
    }
    assert!(!names.iter().any(|choice| choice == "not-a-theme"));
    assert!(
        choices
            .iter()
            .any(|choice| choice.name == "custom"
                && choice.description == "Custom theme from disk")
    );
    assert!(choices.iter().any(|choice| {
        choice.name == "tau-dpc"
            && choice
                .description
                .contains("rad:z66La5YXmV5jbW77ByXvoeTs1c5n")
    }));
}

/// Ensures no-argument `/theme` listings include descriptions when available,
/// while preserving compact name-only output for themes without metadata.
#[test]
fn theme_listing_formats_descriptions_when_present() {
    assert_eq!(
        ThemeChoice {
            name: "custom".to_owned(),
            description: "Custom theme".to_owned(),
        }
        .into_listing_text(),
        "custom — Custom theme"
    );
    assert_eq!(
        ThemeChoice {
            name: "old".to_owned(),
            description: String::new(),
        }
        .into_listing_text(),
        "old"
    );
}

/// Ensures completion/listing metadata extraction remains bounded: oversized
/// external theme files still appear by name but do not block on full parsing
/// or allocation just to discover an optional description.
#[test]
fn available_theme_choices_omit_description_for_oversized_theme_files() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    let mut contents = String::from(r#"{ description: "Too large", styles: {}"#);
    contents.extend(std::iter::repeat_n(' ', 70 * 1024));
    contents.push('}');
    std::fs::write(themes.join("huge.json5"), contents).expect("write oversized theme");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };

    let huge = available_theme_choices(&dirs)
        .into_iter()
        .find(|choice| choice.name == "huge")
        .expect("oversized theme is still listed");

    assert_eq!(huge.description, "");
}

/// Ensures completion/listing metadata extraction does not open non-regular
/// theme entries. They remain visible by name, but with empty descriptions so
/// special files cannot wedge prompt completion or no-argument `/theme` output.
#[test]
fn available_theme_choices_omit_description_for_non_regular_entries() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    std::fs::create_dir(themes.join("directory.json5")).expect("write directory theme entry");
    #[cfg(unix)]
    let special_name = {
        let socket = themes.join("socket.json5");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket theme");
        "socket"
    };
    #[cfg(not(unix))]
    let special_name = "directory";
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };

    let choices = available_theme_choices(&dirs);

    let directory = choices
        .iter()
        .find(|choice| choice.name == "directory")
        .expect("directory theme entry is still listed");
    assert_eq!(directory.description, "");
    let special = choices
        .iter()
        .find(|choice| choice.name == special_name)
        .expect("special theme entry is still listed");
    assert_eq!(special.description, "");
}

/// Ensures malformed external theme files remain available by name for
/// completion/listing even though their optional descriptions cannot be parsed.
#[test]
fn available_theme_choices_omit_description_for_invalid_theme_files() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes dir");
    std::fs::write(themes.join("invalid.json5"), "{ description: ").expect("write invalid theme");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp.path().to_owned()),
        state_dir: None,
    };

    let invalid = available_theme_choices(&dirs)
        .into_iter()
        .find(|choice| choice.name == "invalid")
        .expect("invalid theme is still listed");

    assert_eq!(invalid.description, "");
}

/// Ensures invalid external names fail visibly instead of escaping the themes
/// directory or silently falling back to a built-in theme.
#[test]
fn selected_external_theme_rejects_path_components() {
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(Path::new("/tmp/tau-test").to_owned()),
        state_dir: None,
    };

    let err = select_theme(&dirs, CliTheme::Named("../bad".to_owned())).expect_err("rejects name");

    assert!(err.to_string().contains("invalid theme name"));
}

#[test]
fn display_cwd_replaces_home_prefix() {
    assert_eq!(
        display_cwd(
            Path::new("/home/alice/project"),
            Some(Path::new("/home/alice"))
        ),
        "~/project"
    );
    assert_eq!(
        display_cwd(Path::new("/home/alice"), Some(Path::new("/home/alice"))),
        "~"
    );
    assert_eq!(
        display_cwd(
            Path::new("/home/alice2/project"),
            Some(Path::new("/home/alice"))
        ),
        "/home/alice2/project"
    );
}

#[test]
fn prompt_input_placeholder_keeps_placeholder_style_around_role_style() {
    let theme = tau_themes::Theme::parse(
        r##"
            {
                styles: {
                    "prompt.placeholder": { fg: "dark_grey", italic: true },
                    "status.role": { fg: "cyan", bold: true },
                }
            }
            "##,
    )
    .expect("test theme parses");
    let prompt = prompt_input_placeholder(&theme, Some("engineer"), None, None);
    let spans = prompt.spans();

    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0].text, "Write a message to start a new ");
    assert_eq!(spans[0].style.fg, Some(tau_cli_term::Color::DarkGrey));
    assert!(spans[0].style.italic);
    assert_eq!(spans[1].text, "engineer");
    assert_eq!(spans[1].style.fg, Some(tau_cli_term::Color::Cyan));
    assert!(spans[1].style.bold);
    assert!(spans[1].style.italic);
    assert_eq!(spans[2].text, " agent...");
    assert_eq!(spans[2].style.fg, Some(tau_cli_term::Color::DarkGrey));
    assert!(spans[2].style.italic);

    let prompt = prompt_input_placeholder(
        &theme,
        Some("engineer"),
        Some("engineer_abc"),
        Some((AgentNavigationState::Active, true)),
    );
    let spans = prompt.spans();
    assert_eq!(spans[0].text, "Write a message to ");
    assert_eq!(spans[1].text, "engineer_abc");
    assert_eq!(spans[2].text, "...");
    assert_eq!(spans[2].style.fg, Some(tau_cli_term::Color::DarkGrey));
    assert!(spans[2].style.italic);
}

#[test]
fn suspended_prompt_input_placeholder_explains_explicit_resume() {
    // Regression coverage for the disabled-input copy shown while the selected
    // agent is suspended. The text must make clear that users need to resume it
    // without incorrectly claiming that accepted input changes its mode.
    let theme = tau_themes::Theme::new();
    let prompt = prompt_input_placeholder(
        &theme,
        Some("engineer"),
        Some("engineer_abc"),
        Some((AgentNavigationState::Suspended, false)),
    );
    let text: String = prompt
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert_eq!(
        text,
        "This agent is suspended. Use /resume to include it in navigation."
    );
}

/// The directory and session id form one prompt-context span so theme changes
/// cannot style or hide either half independently.
#[test]
fn right_prompt_context_uses_prompt_cwd_style() {
    let theme = tau_themes::Theme::parse(r##"{ styles: { "prompt.cwd": { fg: "dark_grey" } } }"##)
        .expect("test theme parses");
    let prompt = right_prompt_context(&theme, Path::new("/tmp/project"), None, "session-1");

    assert_eq!(prompt.spans()[0].text, "/tmp/project &session-1");
    assert_eq!(prompt.spans().len(), 1);
    assert_eq!(
        prompt.spans()[0].style.fg,
        Some(tau_cli_term::Color::DarkGrey)
    );
}
