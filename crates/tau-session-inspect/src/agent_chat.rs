//! Human-only conversation projection for durable agent journals.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;
use tau_core::{AgentEntry, AgentJournalSnapshot, AgentTree, NodeId};
use tau_proto::{
    AgentId, ContentPart, ContextItem, ContextRole, Event, PromptSubmissionSource, SessionId,
    UnixMicros,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{AgentTraceError, InspectError};

/// One exported human conversation from the currently selected durable branch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentChat {
    /// Stable export schema name.
    pub schema: &'static str,
    /// Durable agent identity.
    pub agent_id: AgentId,
    /// Agent role recorded at creation.
    pub role: String,
    /// Latest durable display name, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// First available journal record time as RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Every exact session identity observed in durable prompt materialization.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_ids: Vec<SessionId>,
    /// User prompts and assistant responses on the selected branch.
    pub messages: Vec<AgentChatMessage>,
}

impl AgentChat {
    /// Renders the export as a readable Markdown document.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# Tau conversation\n\n");
        if let Some(date) = &self.date {
            output.push_str(&format!("- Date: {date}\n"));
        }
        output.push_str(&format!("- Agent: `{}`\n", self.agent_id));
        output.push_str(&format!("- Role: `{}`\n", self.role));
        if let Some(display_name) = &self.display_name {
            output.push_str(&format!("- Name: {display_name}\n"));
        }
        if !self.session_ids.is_empty() {
            let sessions = self
                .session_ids
                .iter()
                .map(|session| format!("`{session}`"))
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!("- Sessions: {sessions}\n"));
        }
        for message in &self.messages {
            output.push_str("\n## ");
            output.push_str(message.speaker.markdown_heading());
            output.push_str("\n\n");
            output.push_str(&message.text);
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
        output
    }

    /// Serializes the export through Tau's existing strict TOON encoder.
    pub fn to_toon(&self) -> Result<String, InspectError> {
        serde_toon::to_string(self).map_err(|error| {
            InspectError::Trace(AgentTraceError::Projection(format!(
                "failed to serialize conversation TOON: {error}"
            )))
        })
    }

    /// Returns an assistant response by reverse chronological index.
    #[must_use]
    pub fn response_relative(&self, relative_index: usize) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .filter(|message| message.speaker == AgentChatSpeaker::Agent)
            .nth(relative_index)
            .map(|message| message.text.as_str())
    }
}

/// One visible prompt or assistant response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentChatMessage {
    /// Human conversation side.
    pub speaker: AgentChatSpeaker,
    /// Exact visible text.
    pub text: String,
}

/// Human-facing side of one exported message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentChatSpeaker {
    /// Authenticated user prompt.
    User,
    /// Assistant-authored response text.
    Agent,
}

impl AgentChatSpeaker {
    /// Returns the Markdown section title for this side.
    const fn markdown_heading(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Agent => "Agent",
        }
    }
}

/// Loads and validates one durable journal, then projects its selected branch.
pub fn load_agent_chat(agents_dir: &Path, agent_id: &AgentId) -> Result<AgentChat, InspectError> {
    let snapshot = AgentJournalSnapshot::capture(agents_dir, [agent_id.clone()])?;
    let records = snapshot.records(agent_id)?.collect::<Result<Vec<_>, _>>()?;
    project_agent_chat(agent_id, &records)
}

/// Projects a validated record sequence into the human conversation.
fn project_agent_chat(
    agent_id: &AgentId,
    records: &[tau_core::PersistedAgentEvent],
) -> Result<AgentChat, InspectError> {
    let tree = AgentTree::try_from_events(agent_id.clone(), records)
        .map_err(|error| InspectError::Trace(AgentTraceError::Projection(error.to_string())))?;
    let mut conversation_nodes = HashMap::new();
    for record in records {
        if let Some((node_id, speaker)) = conversation_node(&tree, record) {
            conversation_nodes.insert(node_id, speaker);
        }
    }
    let started = records.iter().find_map(|record| match &record.event {
        Event::AgentStarted(started) => Some(started),
        _ => None,
    });
    let role = started
        .map(|started| started.role.clone())
        .unwrap_or_default();
    let date = records
        .first()
        .filter(|record| record.recorded_at.get() != 0)
        .map(|record| format_unix_micros(record.recorded_at))
        .transpose()?;
    let session_ids = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptStarted(started) => Some(started.session_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let messages = tree
        .branch_node_ids_from(tree.head())
        .into_iter()
        .filter_map(|node_id| {
            let kind = conversation_nodes.get(&node_id)?;
            tree.node(node_id)
                .and_then(|node| project_entry(&node.entry, *kind))
        })
        .collect();
    Ok(AgentChat {
        schema: "tau.agent_chat",
        agent_id: agent_id.clone(),
        role,
        display_name: tree.display_name().map(str::to_owned),
        date,
        session_ids,
        messages,
    })
}

/// Retains the durable prompt class and response originator that `AgentEntry`
/// intentionally omits from its provider-context representation.
fn conversation_node(
    tree: &AgentTree,
    record: &tau_core::PersistedAgentEvent,
) -> Option<(NodeId, AgentChatSpeaker)> {
    match &record.event {
        Event::AgentPromptSubmitted(prompt)
            if !prompt.message_class.is_internal()
                && prompt.originator.is_user()
                && matches!(
                    prompt.submission_source,
                    PromptSubmissionSource::HumanUi | PromptSubmissionSource::Legacy
                ) =>
        {
            tree.node_for_durable_event_seq(record.seq)
                .map(|node_id| (node_id, AgentChatSpeaker::User))
        }
        Event::AgentPromptSteered(prompt)
            if !prompt.message_class.is_internal()
                && matches!(
                    prompt.submission_source,
                    PromptSubmissionSource::HumanUi | PromptSubmissionSource::Legacy
                ) =>
        {
            tree.node_for_durable_event_seq(record.seq)
                .map(|node_id| (node_id, AgentChatSpeaker::User))
        }
        Event::ProviderResponseFinished(response) if response.originator.is_user() => tree
            .assistant_response_node_for_durable_event_seq(record.seq)
            .map(|node_id| (node_id, AgentChatSpeaker::Agent)),
        _ => None,
    }
}

/// Keeps only authenticated human prompts and assistant-authored visible text.
fn project_entry(entry: &AgentEntry, speaker: AgentChatSpeaker) -> Option<AgentChatMessage> {
    match (entry, speaker) {
        (AgentEntry::UserInput { items, .. }, AgentChatSpeaker::User) => {
            visible_message_text(items, ContextRole::User)
                .map(|text| AgentChatMessage { speaker, text })
        }
        (AgentEntry::AssistantResponse { output_items, .. }, AgentChatSpeaker::Agent) => {
            visible_message_text(output_items, ContextRole::Assistant)
                .map(|text| AgentChatMessage { speaker, text })
        }
        (
            AgentEntry::UserInput { .. }
            | AgentEntry::AssistantResponse { .. }
            | AgentEntry::ToolResults { .. }
            | AgentEntry::AgentMessage { .. }
            | AgentEntry::MessageFact { .. }
            | AgentEntry::Compaction { .. }
            | AgentEntry::CompactionTrigger { .. },
            _,
        ) => None,
    }
}

/// Concatenates ordinary text parts from messages with the requested role.
fn visible_message_text(items: &[ContextItem], role: ContextRole) -> Option<String> {
    let messages = items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(message) if message.role == role => Some(
                message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::SyntheticCompactionSummary { .. }
                        | ContentPart::HarnessInternalText { .. }
                        | ContentPart::UrlCitation { .. }
                        | ContentPart::CitationMetadataInvalid => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

/// Formats an authoritative journal timestamp without inventing timezone
/// context.
fn format_unix_micros(timestamp: UnixMicros) -> Result<String, InspectError> {
    let nanos = i128::from(timestamp.get()) * 1_000;
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|error| {
        InspectError::Trace(AgentTraceError::Projection(format!(
            "invalid conversation timestamp: {error}"
        )))
    })?;
    timestamp.format(&Rfc3339).map_err(|error| {
        InspectError::Trace(AgentTraceError::Projection(format!(
            "could not format conversation timestamp: {error}"
        )))
    })
}

#[cfg(test)]
mod tests;
