//! Scoped Rostra following-notification preference tool.

use tau_client::{ClientError, ClientResult};

use crate::{RostraState, tools};

/// Strict wire arguments for one scoped notification-preference mutation.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Requested per-agent notification preference.
    enabled: bool,
}

/// Persists one agent-scoped notification preference before tool success.
pub(crate) fn handle(cx: tau_client::ToolContext<'_, RostraState>) -> ClientResult<()> {
    let arguments = serde_json::to_value(&cx.invoke().arguments)
        .map_err(|_| ClientError::handler("invalid_argument: arguments are not an object"))?;
    let args: Args = serde_json::from_value(arguments).map_err(|_| {
        ClientError::handler("invalid_argument: arguments do not match the tool schema")
    })?;
    let Some(client) = cx.state.client.clone() else {
        let event = tools::tool_error(cx.invoke(), tools::ToolFailure::not_ready());
        let outcome = tau_client::ToolTerminalOutcome::try_from(event)
            .map_err(|_| ClientError::handler("internal_failure: invalid terminal event"))?;
        return cx.handle().report_tool_terminal_detached(outcome);
    };
    let agent_id = cx.invoke().agent_id.clone();
    let preference = if args.enabled {
        let baseline = cx
            .state
            .runtime
            .as_ref()
            .expect("runtime exists until state drop")
            .block_on(client.db().get_social_post_materialization_tip())
            .map_err(|_| {
                ClientError::handler("storage_failure: could not read Rostra materialization tip")
            })?;
        Preference::Enable(baseline)
    } else {
        Preference::Disable
    };
    persist_notification_preference(cx, &agent_id, preference)
}

/// One validated notification-preference mutation ready for durable
/// persistence.
enum Preference {
    /// Enable from this exact materialization baseline.
    Enable(rostra_client::SocialPostMaterializationCursor),
    /// Remove the durable preference.
    Disable,
}

/// Commits the local policy mutation and emits the normal tool terminal result.
fn persist_notification_preference(
    cx: tau_client::ToolContext<'_, RostraState>,
    agent_id: &tau_proto::AgentId,
    preference: Preference,
) -> ClientResult<()> {
    let enabled = matches!(preference, Preference::Enable(_));
    let mut notifications =
        cx.state.notifications.lock().map_err(|_| {
            ClientError::handler("internal_failure: notification state is unavailable")
        })?;
    match preference {
        Preference::Enable(baseline) => notifications.enable(agent_id.clone(), baseline),
        Preference::Disable => notifications.disable(agent_id),
    }
    .map_err(|message| ClientError::handler(format!("storage_failure: {message}")))?;
    drop(notifications);
    cx.state.notifications_wake.notify_one();
    let event = tools::tool_result(
        cx.invoke(),
        if enabled {
            "Rostra following notifications registered; future receipt-ordered posts will be batched."
        } else {
            "Rostra following notifications unregistered."
        }
        .to_owned(),
    );
    let outcome = tau_client::ToolTerminalOutcome::try_from(event)
        .map_err(|_| ClientError::handler("internal_failure: invalid terminal event"))?;
    cx.handle().report_tool_terminal_detached(outcome)
}
