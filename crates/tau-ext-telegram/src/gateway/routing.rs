//! Routing helpers for the standalone Telegram gateway.

use super::GatewayRegistrationKey;
use crate::TgMessage;

/// Deterministic live registry snapshot used by command routing.
pub(super) struct GatewayRegistrySnapshot {
    /// Live sessions visible to the gateway.
    pub(super) sessions: Vec<GatewaySessionSnapshot>,
    /// Live agent registrations visible to the gateway.
    pub(super) registrations: Vec<GatewayRegistrationSnapshot>,
}

impl GatewayRegistrySnapshot {
    /// Resolve a session alias or stable id prefix.
    pub(super) fn resolve_session(&self, selector: &str) -> Result<String, String> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("Session selector is required.".to_owned());
        }
        if let Some(alias) = parse_numbered_alias(selector, 's') {
            return self
                .sessions
                .iter()
                .find(|session| session.alias == alias)
                .map(|session| session.session_id.clone())
                .ok_or_else(|| "Unknown session alias.".to_owned());
        }
        let matches = self
            .sessions
            .iter()
            .filter(|session| session.session_id.starts_with(selector))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [session] => Ok(session.session_id.clone()),
            [] => Err("No live session matches that selector.".to_owned()),
            _ => Err("Session selector is ambiguous; use a /sessions alias.".to_owned()),
        }
    }

    /// Resolve an agent alias or stable id prefix within one session.
    pub(super) fn resolve_agent_in_session(
        &self,
        session_id: &str,
        selector: &str,
    ) -> Result<String, String> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("Agent selector is required.".to_owned());
        }
        let agents = self.agents_in_session(session_id);
        if let Some(alias) = parse_numbered_alias(selector, 'a') {
            return agents
                .iter()
                .find(|agent| agent.alias == alias)
                .map(|agent| agent.agent_id.clone())
                .ok_or_else(|| "Unknown agent alias.".to_owned());
        }
        let matches = agents
            .iter()
            .filter(|agent| agent.agent_id.starts_with(selector))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [agent] => Ok(agent.agent_id.clone()),
            [] => Err("No live agent matches that selector.".to_owned()),
            _ => Err("Agent selector is ambiguous; use an /agents alias.".to_owned()),
        }
    }

    /// Return sorted agents in one live session.
    pub(super) fn agents_in_session(&self, session_id: &str) -> Vec<GatewayAgentView> {
        self.registrations
            .iter()
            .filter(|registration| registration.key.session_id == session_id)
            .map(|registration| GatewayAgentView {
                session_id: registration.key.session_id.clone(),
                agent_id: registration.key.agent_id.clone(),
                display_name: registration.display_name.clone(),
                alias: registration.alias,
            })
            .collect()
    }

    /// Return true when a target registration is currently live.
    pub(super) fn has_registration(&self, session_id: &str, agent_id: &str) -> bool {
        self.registrations.iter().any(|registration| {
            registration.key.session_id == session_id && registration.key.agent_id == agent_id
        })
    }

    /// Return the privacy-preserving label for a session.
    pub(super) fn session_label(&self, session_id: &str) -> String {
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .map(|session| session_alias(session.alias))
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

/// One live session in a registry snapshot.
pub(super) struct GatewaySessionSnapshot {
    /// Full Tau session id retained internally for routing.
    pub(super) session_id: String,
    /// Stable gateway-local numeric alias.
    pub(super) alias: usize,
    /// Number of live agents in this session.
    pub(super) agent_count: usize,
}

/// One live registration in a registry snapshot.
pub(super) struct GatewayRegistrationSnapshot {
    /// Full route key retained internally for routing.
    pub(super) key: GatewayRegistrationKey,
    /// Optional display name supplied by the sidecar.
    pub(super) display_name: Option<String>,
    /// Stable gateway-local numeric agent alias.
    pub(super) alias: usize,
}

/// Agent view used while rendering `/agents` output.
pub(super) struct GatewayAgentView {
    /// Full Tau session id retained internally for routing.
    pub(super) session_id: String,
    /// Full Tau agent id retained internally for routing.
    pub(super) agent_id: String,
    /// Optional display name supplied by the sidecar.
    pub(super) display_name: Option<String>,
    /// Stable gateway-local numeric agent alias.
    pub(super) alias: usize,
}

/// Prompt delivery queued for a sidecar client.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct GatewayDelivery {
    /// Gateway-minted request id for this live queued delivery.
    pub(super) request_id: String,
    /// Target Tau session id.
    pub(super) session_id: String,
    /// Target Tau agent id.
    pub(super) agent_id: String,
    /// Sanitized Telegram source label.
    pub(super) source: String,
    /// Prompt text including the Telegram source prefix.
    pub(super) text: String,
    /// Gateway-minted context id for dedup/log correlation.
    pub(super) ctx_id: String,
}

/// Split a `/to` body into target selector and non-empty prompt text.
pub(super) fn split_target_and_text(input: &str) -> Option<(&str, &str)> {
    let input = input.trim();
    let (target, text) = input.split_once(char::is_whitespace)?;
    let text = text.trim();
    (!target.trim().is_empty() && !text.is_empty()).then_some((target.trim(), text))
}

/// Return a one-based session alias.
pub(super) fn session_alias(number: usize) -> String {
    format!("s{number}")
}

/// Return a one-based agent alias.
pub(super) fn agent_alias(number: usize) -> String {
    format!("a{number}")
}

/// Return a short stable-id prefix for diagnostics without dumping long ids.
pub(super) fn short_id(value: &str) -> String {
    const MAX: usize = 12;
    if value.chars().count() <= MAX {
        return value.to_owned();
    }
    let mut end = 0;
    for (count, (index, ch)) in value.char_indices().enumerate() {
        if count == MAX {
            break;
        }
        end = index + ch.len_utf8();
    }
    format!("{}…", &value[..end])
}

/// Sanitize sidecar-supplied display metadata before including it in replies.
pub(super) fn safe_metadata(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Build a bounded, sanitized source label for routed Telegram prompts.
pub(super) fn telegram_source_label(message: &TgMessage) -> String {
    message
        .from_name
        .as_deref()
        .map(safe_metadata)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("user {}", message.user_id))
}

/// Parse a one-based `sN` or `aN` alias into its numeric value.
fn parse_numbered_alias(selector: &str, prefix: char) -> Option<usize> {
    let suffix = selector.strip_prefix(prefix)?;
    let number = suffix.parse::<usize>().ok()?;
    (number > 0).then_some(number)
}
