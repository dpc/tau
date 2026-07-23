//! User-tweakable UI settings exposed through `:set <name> <value>`.
//!
//! Each [`SettingDef`] knows how to read/write a field on [`CliState`]
//! and what its allowed values are. The registry drives both the
//! `:set` parser and completion (setting names with current values,
//! then values with descriptions).
//!
//! Most settings are booleans rendered as `true`/`false`; the shape is
//! value-list based so settings can also take three or more named values
//! without further plumbing. Numeric settings can provide suggested values
//! while accepting any value that passes their validator.

use tau_config::settings::CliState;

/// One allowed value for a setting, with a short description shown in
/// the completion menu.
pub struct SettingValue {
    pub value: &'static str,
    pub description: &'static str,
}

/// Definition of a `:set`-controllable UI setting.
pub struct SettingDef {
    pub name: &'static str,
    pub description: &'static str,
    pub value_hint: &'static str,
    pub values: &'static [SettingValue],
    pub validate: fn(&str) -> bool,
    /// Read the setting's current value from `CliState`, returning the
    /// matching `values[i].value` string. Used by the completion menu
    /// to show the live value alongside each setting name. Writes go
    /// through the renderer's per-setting repaint dispatch instead of
    /// a generic setter — every setting needs a distinct invalidation
    /// (re-render diff blocks vs. status bar vs. turn-stats blocks)
    /// so a `fn(&mut CliState, &str)` here wouldn't actually buy us
    /// anything.
    pub get: fn(&CliState) -> String,
}

const BOOL_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "true",
        description: "enabled",
    },
    SettingValue {
        value: "false",
        description: "disabled",
    },
];

const SHOW_MESSAGES_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "none",
        description: "hide agent-agent messages; user messages still show",
    },
    SettingValue {
        value: "self-summary",
        description: "hide agent-agent messages; user messages still show",
    },
    SettingValue {
        value: "self-full",
        description: "hide agent-agent messages; user messages still show",
    },
    SettingValue {
        value: "all-summary",
        description: "show user messages, summarize agent-agent messages",
    },
    SettingValue {
        value: "all-full",
        description: "show all messages",
    },
];

const SHOW_TOOLS_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "off",
        description: "hide tool blocks",
    },
    SettingValue {
        value: "summarize-turn",
        description: "show one summary per assistant tool turn",
    },
    SettingValue {
        value: "summarize-prompt",
        description: "show one summary per user prompt",
    },
    SettingValue {
        value: "compact",
        description: "show tool headers without payloads",
    },
    SettingValue {
        value: "full",
        description: "show every tool block",
    },
];

const NOTICE_LEVEL_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "critical",
        description: "show only critical harness failures",
    },
    SettingValue {
        value: "warning",
        description: "show warnings and critical notices",
    },
    SettingValue {
        value: "info",
        description: "show useful notices, warnings, and critical failures",
    },
    SettingValue {
        value: "debug",
        description: "also show debugging notices",
    },
    SettingValue {
        value: "trace",
        description: "show developer-only trace notices",
    },
];

fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

fn validate_bool(value: &str) -> bool {
    matches!(value, "true" | "false")
}

fn validate_list(value: &str, values: &[SettingValue]) -> bool {
    values.iter().any(|v| v.value == value)
}

fn validate_redraw_history_size(value: &str) -> bool {
    value.parse::<usize>().is_ok()
}

const REDRAW_HISTORY_SIZE_VALUES: &[SettingValue] = &[
    SettingValue {
        value: "0",
        description: "replay no off-screen history on full redraw",
    },
    SettingValue {
        value: "100",
        description: "small mobile/slow-SSH history replay",
    },
    SettingValue {
        value: "500",
        description: "moderate history replay",
    },
    SettingValue {
        value: "2000",
        description: "default history replay",
    },
    SettingValue {
        value: "10000",
        description: "large history replay",
    },
];

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        name: "show-diff",
        description: "Expanded vs compact display of file edit diffs",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.show_diff).to_owned(),
    },
    SettingDef {
        name: "show-thinking",
        description: "Visibility of the agent's reasoning summary blocks",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.show_thinking).to_owned(),
    },
    SettingDef {
        name: "show-turn-stats",
        description: "Turn stats below agent responses",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.show_turn_stats).to_owned(),
    },
    SettingDef {
        name: "redraw-counter",
        description: "Temporary full-redraw counter in the status bar",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.redraw_counter).to_owned(),
    },
    SettingDef {
        name: "redraw-history-size",
        description: "History lines replayed on full redraw",
        value_hint: "non-negative integer",
        values: REDRAW_HISTORY_SIZE_VALUES,
        validate: validate_redraw_history_size,
        get: |s| s.redraw_history_size.to_string(),
    },
    SettingDef {
        name: "show-ui-io",
        description: "UI↔harness socket throughput in the status bar",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.show_ui_io).to_owned(),
    },
    SettingDef {
        name: "show-tools",
        description: "Tool block visibility",
        value_hint: "off|summarize-turn|summarize-prompt|compact|full",
        values: SHOW_TOOLS_VALUES,
        validate: |value| validate_list(value, SHOW_TOOLS_VALUES),
        get: |s| s.show_tools.as_str().to_owned(),
    },
    SettingDef {
        name: "show-messages",
        description: "Agent message visibility",
        value_hint: "none|self-summary|self-full|all-summary|all-full",
        values: SHOW_MESSAGES_VALUES,
        validate: |value| validate_list(value, SHOW_MESSAGES_VALUES),
        get: |s| s.show_messages.as_str().to_owned(),
    },
    SettingDef {
        name: "notice-level",
        description: "Harness/UI notice visibility threshold",
        value_hint: "critical|warning|info|debug|trace",
        values: NOTICE_LEVEL_VALUES,
        validate: |value| validate_list(value, NOTICE_LEVEL_VALUES),
        get: |s| s.notice_level.as_str().to_owned(),
    },
    SettingDef {
        name: "show-prompt-scroll-indicator",
        description: "Hidden-row indicator for capped prompt input",
        value_hint: "true|false",
        values: BOOL_VALUES,
        validate: validate_bool,
        get: |s| bool_str(s.show_prompt_scroll_indicator).to_owned(),
    },
];

pub fn find(name: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests;
