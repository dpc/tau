/// `:set show-messages` is registry-driven, so the registry must expose all
/// documented modes for parsing and completion.
#[test]
fn show_messages_values_are_registered() {
    let setting = super::find("show-messages").expect("show-messages setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(
        values,
        vec![
            "none",
            "self-summary",
            "self-full",
            "all-summary",
            "all-full"
        ]
    );
}

/// `:set show-internal-prompts` uses the compact on/off vocabulary promised by
/// the runtime setting contract.
#[test]
fn show_internal_prompts_values_are_registered() {
    let setting = super::find("show-internal-prompts").expect("show-internal-prompts setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(values, vec!["on", "off"]);
    assert!((setting.validate)("on"));
    assert!(!(setting.validate)("true"));
}

/// `:set show-ui-io` is a boolean status-bar toggle, so it should use the
/// standard true/false values that completion and validation expect.
#[test]
fn show_ui_io_values_are_registered() {
    let setting = super::find("show-ui-io").expect("show-ui-io setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(values, vec!["true", "false"]);
}

/// `:set notice-level` is ordered by visibility threshold and uses meaningful
/// severity names for completion and validation.
#[test]
fn notice_level_values_are_registered() {
    let setting = super::find("notice-level").expect("notice-level setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(
        values,
        vec!["critical", "warning", "info", "debug", "trace"]
    );
}

/// `:set show-prompt-scroll-indicator` is a boolean prompt-input toggle.
#[test]
fn show_prompt_scroll_indicator_values_are_registered() {
    let setting =
        super::find("show-prompt-scroll-indicator").expect("show-prompt-scroll-indicator setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(values, vec!["true", "false"]);
}

/// `:set redraw-history-size` accepts arbitrary non-negative integers while
/// still offering common sizes in completion.
#[test]
fn redraw_history_size_accepts_integer_values() {
    let setting = super::find("redraw-history-size").expect("redraw-history-size setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert!(values.contains(&"2000"));
    assert!((setting.validate)("0"));
    assert!((setting.validate)("12345"));
    assert!(!(setting.validate)("all"));
    assert!(!(setting.validate)("-1"));
}
