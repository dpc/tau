//! Shared inner-turn presentation helpers for agent-status views.

use tau_themes::names;

use super::*;

/// Builds the independently themed watched-agent suffix from available stats.
pub(super) fn watched_agent_suffix(
    stats: Option<&tau_proto::AgentStatsUpdated>,
) -> Option<ToolLineSegment> {
    stats
        .and_then(|stats| stats.inner_turns_total)
        .map(|inner_turns_total| ToolLineSegment {
            text: format!("*{inner_turns_total}"),
            status: ToolStatus::InnerTurns,
            no_leading_space: false,
        })
}

/// Builds the selected-agent inner-turn status chip from a complete stats
/// snapshot.
pub(super) fn selected_agent_status_chip(
    theme: &tau_themes::Theme,
    selected_agent_id: Option<&tau_proto::AgentId>,
    agent_stats: &HashMap<tau_proto::AgentId, tau_proto::AgentStatsUpdated>,
) -> Option<tau_cli_term::StyledText> {
    let inner_turns_total = selected_agent_id
        .and_then(|agent_id| agent_stats.get(agent_id))
        .and_then(|stats| stats.inner_turns_total)?;
    Some(status_chip(
        theme,
        names::STATUS_INNER_TURNS,
        format!("*{inner_turns_total}"),
    ))
}
