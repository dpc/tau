//! Shared completion values and descriptions for portable effort controls.

const ABSOLUTE_VALUES: &[&str] = &[
    "reset",
    "provider_default",
    "disabled",
    "0.0",
    "0.25",
    "0.5",
    "0.75",
    "1.0",
];

/// Builds one effort completion with context-specific reset wording.
pub(super) fn value(value: &str, reset_description: &'static str) -> tau_cli_term::CompletionItem {
    let description = match value {
        "reset" => reset_description,
        "provider_default" => "omit effort and use the provider default",
        "disabled" => "request disabled reasoning",
        "0.0" => "minimum portable reasoning intensity",
        "0.25" => "light portable reasoning intensity",
        "0.5" => "medium-like portable reasoning intensity",
        "0.75" => "strong portable reasoning intensity",
        "1.0" => "maximum portable reasoning intensity",
        "increase:0.25" => "increase portable intensity by 0.25",
        "decrease:0.25" => "decrease portable intensity by 0.25",
        _ => "",
    };
    tau_cli_term::CompletionItem::new(value, description)
}

/// Completes the absolute effort grammar accepted by agent-local `:effort`.
pub(super) fn absolute_values(needle: &str) -> Vec<tau_cli_term::CompletionItem> {
    ABSOLUTE_VALUES
        .iter()
        .copied()
        .filter(|value| needle.is_empty() || value.starts_with(needle) || value.contains(needle))
        .map(|candidate| value(candidate, "clear this agent effort override"))
        .collect()
}
