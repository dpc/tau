use super::*;

/// Ensures error chips use the trimmed first meaningful line, leaving later
/// detail and any renderer-specific abbreviation outside tool-side formatting.
#[test]
fn error_chip_text_keeps_full_first_line_for_renderer_abbreviation() {
    let message = "\n \t\n  failed to load extension configuration because the selected profile \
                   references a missing credential source and requires operator action before \
                   retrying  \nignored detail";
    let failure = ToolFailure::new(message);

    assert_eq!(
        failure.display.status_text,
        "failed to load extension configuration because the selected profile references a missing \
         credential source and requires operator action before retrying"
    );
    assert!(!failure.display.status_text.contains("ignored detail"));
}
