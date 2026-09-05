//! Bounded prompt, response, message-label, and timer projections.

use crate::event_renderer::TIMER_WAKEUP_CTX_PREFIX;
use crate::markdown_render::markdown_block_with_osc8;

/// Prefix for completed assistant response rows.
pub(super) const COMPLETED_AGENT_RESPONSE_PREFIX: &str = "◆ ";
/// Prefix for streaming assistant response rows.
pub(super) const STREAMING_AGENT_RESPONSE_PREFIX: &str = "◇ ";
/// Maximum rendered terminal columns for a supplemental agent message name.
pub(super) const AGENT_MESSAGE_NAME_MAX_COLUMNS: usize = 48;
/// Maximum rendered UTF-8 bytes for a supplemental agent message name.
pub(super) const AGENT_MESSAGE_NAME_MAX_BYTES: usize = 192;
/// Maximum bytes inspected at either end of a queued prompt.
pub(super) const QUEUED_PROJECTION_WINDOW_BYTES: usize = 16 * 1024;

/// Returns the bounded first logical line of queued prompt text.
pub(super) fn bounded_queued_line_start(text: &str) -> &str {
    let mut end = text.len().min(QUEUED_PROJECTION_WINDOW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let window = &text[..end];
    window
        .find(['\n', '\r'])
        .map_or(window, |line_end| &window[..line_end])
}

/// Returns the bounded final logical line of queued prompt text.
pub(super) fn bounded_queued_line_end(text: &str) -> &str {
    let mut start = text.len().saturating_sub(QUEUED_PROJECTION_WINDOW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let window = &text[start..];
    window
        .rfind(['\n', '\r'])
        .map_or(window, |line_end| &window[line_end + 1..])
}

/// Builds the bounded two-line queued-prompt terminal projection.
pub(super) fn queued_prompt_projection(
    theme: &tau_themes::Theme,
    osc8_links: bool,
    prefix: tau_cli_term::StyledText,
    text: &str,
) -> tau_cli_term::TwoLineElision {
    let styled = |value| {
        markdown_block_with_osc8(
            theme,
            tau_themes::names::USER_PROMPT_QUEUED,
            value,
            osc8_links,
        )
        .content
    };
    let unabridged_text =
        (text.len() <= QUEUED_PROJECTION_WINDOW_BYTES).then(|| format!("{text} (queued)"));
    let unabridged = unabridged_text.as_deref().map(styled);
    tau_cli_term::TwoLineElision {
        prefix,
        first: styled(bounded_queued_line_start(text)),
        last: styled(bounded_queued_line_end(text)),
        first_omissions: vec![styled("   ┄"), styled("┄")],
        last_omissions: vec![styled("┄ "), styled("┄")],
        labels: vec![styled(" (queued)"), styled(" (q)"), styled("q")],
        unabridged,
    }
}

/// Parses the timer id and occurrence count from a wakeup context id.
pub(super) fn timer_wakeup_ctx(ctx_id: Option<&str>) -> Option<(&str, &str)> {
    let rest = ctx_id?.strip_prefix(TIMER_WAKEUP_CTX_PREFIX)?;
    rest.rsplit_once(':')
}

/// Builds user-visible timer wakeup summary text.
pub(super) fn timer_wakeup_summary(timer_id: &str, text: Option<&str>) -> String {
    let Some(text) = text else {
        return format!("Timer `{timer_id}` woke this agent");
    };
    let trimmed = text.trim();
    let timer_prefix = format!("Timer `{timer_id}` fired:");
    let message = trimmed
        .strip_prefix(&timer_prefix)
        .map(str::trim)
        .unwrap_or(trimmed);
    if message.is_empty() {
        format!("Timer `{timer_id}` woke this agent")
    } else {
        format!("Timer `{timer_id}` woke this agent: {message}")
    }
}
