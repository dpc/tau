/// `/set show-messages` is registry-driven, so the registry must expose all
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

/// `/set show-ui-io` is a boolean status-bar toggle, so it should use the
/// standard true/false values that completion and validation expect.
#[test]
fn show_ui_io_values_are_registered() {
    let setting = super::find("show-ui-io").expect("show-ui-io setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(values, vec!["true", "false"]);
}

/// `/set notice-level` is ordered by visibility threshold and uses meaningful
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

/// `/set show-prompt-scroll-indicator` is a boolean prompt-input toggle.
#[test]
fn show_prompt_scroll_indicator_values_are_registered() {
    let setting =
        super::find("show-prompt-scroll-indicator").expect("show-prompt-scroll-indicator setting");
    let values: Vec<_> = setting.values.iter().map(|value| value.value).collect();

    assert_eq!(values, vec!["true", "false"]);
}
