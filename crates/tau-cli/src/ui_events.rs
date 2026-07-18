//! Shared constructors for UI protocol events.

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tau_proto::{Event, UiRoleUpdateAction};

pub(crate) fn cancel_prompt(
    session_id: &str,
    target_agent_id: Option<tau_proto::AgentId>,
) -> Event {
    // Broadcast cancel within the selected agent conversation — abort whatever
    // is in flight there, regardless of spid. The targeted variant is used by
    // the harness for surgical preempts.
    Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
        session_id: session_id.into(),
        target_agent_id,
        agent_prompt_id: None,
    })
}

pub(crate) fn retry_prompt(session_id: &str, target_agent_id: Option<tau_proto::AgentId>) -> Event {
    static NEXT_RETRY_ID: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    // Fixed-width process/time/counter IDs avoid wall-clock collisions while
    // remaining valid across independent UI processes.
    let nonce = time
        ^ u64::from(process::id()).rotate_left(32)
        ^ NEXT_RETRY_ID.fetch_add(1, Ordering::Relaxed);
    Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
        request_id: tau_proto::RetryPromptRequestId::parse(format!("{nonce:016x}"))
            .expect("valid retry request id"),
        session_id: session_id.into(),
        target_agent_id,
        agent_prompt_id: None,
    })
}

pub(crate) fn tree_request(session_id: &str, target_agent_id: Option<tau_proto::AgentId>) -> Event {
    Event::UiTreeRequest(tau_proto::UiTreeRequest {
        session_id: session_id.into(),
        target_agent_id,
    })
}

pub(crate) fn navigate_tree(
    session_id: &str,
    target_agent_id: Option<tau_proto::AgentId>,
    target: tau_proto::UiTreeNavigationTarget,
) -> Event {
    Event::UiNavigateTree(tau_proto::UiNavigateTree {
        session_id: session_id.into(),
        target_agent_id,
        target,
    })
}

pub(crate) fn compact_request(
    session_id: &str,
    target_agent_id: Option<tau_proto::AgentId>,
) -> Event {
    Event::UiCompactRequest(tau_proto::UiCompactRequest {
        session_id: session_id.into(),
        target_agent_id,
    })
}

pub(crate) fn role_select(role: impl Into<String>) -> Event {
    Event::UiRoleSelect(tau_proto::UiRoleSelect { role: role.into() })
}

pub(crate) fn agent_model_select(
    session_id: &str,
    target_agent_id: Option<tau_proto::AgentId>,
    model: tau_proto::ModelId,
) -> Event {
    Event::UiAgentModelSelect(tau_proto::UiAgentModelSelect {
        session_id: session_id.into(),
        target_agent_id,
        model,
    })
}

pub(crate) fn set_agent_display_name(
    session_id: &str,
    agent_id: tau_proto::AgentId,
    display_name: impl Into<String>,
) -> Event {
    Event::UiSetAgentDisplayName(tau_proto::UiSetAgentDisplayName {
        session_id: session_id.into(),
        agent_id,
        display_name: display_name.into(),
    })
}

/// Build one uniquely correlated absolute shared-navigation mutation.
pub(crate) fn set_agent_navigation_mode(
    session_id: &str,
    agent_id: tau_proto::AgentId,
    action: tau_proto::UiAgentNavigationModeAction,
) -> Event {
    static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let nonce = time
        ^ u64::from(process::id()).rotate_left(32)
        ^ NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    Event::UiSetAgentNavigationMode(tau_proto::UiSetAgentNavigationMode {
        request_id: format!("{nonce:016x}"),
        session_id: session_id.into(),
        agent_id,
        action,
    })
}

pub(crate) fn role_update(role: impl Into<String>, action: UiRoleUpdateAction) -> Event {
    Event::UiRoleUpdate(tau_proto::UiRoleUpdate {
        role: role.into(),
        action,
    })
}

pub(crate) fn shell_command(
    session_id: &str,
    command: &str,
    include_in_context: bool,
    target_agent_id: Option<tau_proto::AgentId>,
) -> Event {
    let command_id = format!(
        "ui-sh-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: session_id.into(),
        command_id: command_id.into(),
        command: command.to_owned(),
        include_in_context,
        target_agent_id,
    })
}

#[cfg(test)]
mod tests;
