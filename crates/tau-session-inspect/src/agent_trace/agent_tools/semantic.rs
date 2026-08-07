//! Typed assistant, reasoning, and directional-message projection.

use base64::{Engine as _, engine as path_base64_engine};
use serde::Serialize;
use tau_proto::{
    AgentMessageKind, AgentMessageRecipient, ContentPart, ContextItem, ContextRole, Event,
};

use super::{super as path_super_super, RecordRank};

/// One typed semantic family payload.
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(super) enum SemanticRecord {
    /// Assistant prose from a canonical provider finish.
    AssistantMessage(AssistantMessageRecord),
    /// Displayable reasoning from a canonical provider finish.
    AssistantReasoning(AssistantReasoningRecord),
    /// Canonical accepted sender-side message.
    MessageSent(MessageSentRecord),
    /// Canonical accepted recipient-side message.
    MessageReceived(MessageReceivedRecord),
}

/// One semantic record paired with its stable family sort rank.
pub(super) struct SemanticProjection {
    /// Typed semantic payload.
    pub(super) record: SemanticRecord,
    /// Stable family rank.
    pub(super) rank: RecordRank,
}

/// Complete metrics plus selected semantic text.
#[derive(Clone, Serialize)]
pub(super) struct TextProjection {
    /// Complete canonical UTF-8 byte count.
    text_bytes: usize,
    /// Complete canonical Rust `str::lines` count.
    text_lines: usize,
    /// Complete or bounded selected text.
    text: String,
    /// Whether selected text is complete.
    text_complete: bool,
}

impl TextProjection {
    /// Selects bounded or complete text while retaining complete metrics.
    fn new(text: &str, mode: super::super::AgentTraceMode) -> Self {
        let (selected, complete) = match mode {
            path_super_super::AgentTraceMode::Lite => super::lite_output(text),
            path_super_super::AgentTraceMode::Full => (text, true),
        };
        Self {
            text_bytes: text.len(),
            text_lines: text.lines().count(),
            text: selected.to_owned(),
            text_complete: complete,
        }
    }
}

/// Assistant prose occurrence.
#[derive(Clone, Serialize)]
pub(super) struct AssistantMessageRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Canonical provider prompt identity.
    agent_prompt_id: tau_proto::AgentPromptId,
    /// Provider-declared semantic phase, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<tau_proto::MessagePhase>,
    /// Projected assistant text and complete metrics.
    #[serde(flatten)]
    text: TextProjection,
}

/// Displayable provider reasoning occurrence.
#[derive(Clone, Serialize)]
pub(super) struct AssistantReasoningRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Canonical provider prompt identity.
    agent_prompt_id: tau_proto::AgentPromptId,
    /// Exact displayable reasoning kind.
    reasoning_kind: tau_proto::ReasoningTextKind,
    /// Projected reasoning text and complete metrics.
    #[serde(flatten)]
    text: TextProjection,
}

/// Typed sender-side recipient projection.
#[derive(Clone, Serialize)]
#[serde(tag = "recipient_kind", rename_all = "snake_case")]
pub(super) enum MessageRecipientRecord {
    /// Same-session agent recipient.
    Agent {
        /// Recipient agent identity.
        recipient_id: tau_proto::AgentId,
    },
    /// Cross-session agent recipient.
    ExternalAgent {
        /// Recipient agent identity.
        recipient_id: tau_proto::AgentId,
        /// Historical recipient session identity.
        recipient_session_id: tau_proto::SessionId,
    },
    /// Human user recipient.
    User,
}

/// Canonical sender-side message occurrence.
#[derive(Clone, Serialize)]
pub(super) struct MessageSentRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Stable logical message identity.
    message_id: tau_proto::AgentMessageId,
    /// Canonical sender identity.
    sender_id: tau_proto::AgentId,
    /// Typed recipient identity.
    #[serde(flatten)]
    recipient: MessageRecipientRecord,
    /// Projected message body and complete metrics.
    #[serde(flatten)]
    text: TextProjection,
}

/// Canonical recipient-side message occurrence.
#[derive(Clone, Serialize)]
pub(super) struct MessageReceivedRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Stable logical message identity.
    message_id: tau_proto::AgentMessageId,
    /// Canonical sender identity.
    sender_id: tau_proto::AgentId,
    /// Historical external sender session, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_session_id: Option<tau_proto::SessionId>,
    /// Canonical recipient identity.
    recipient_id: tau_proto::AgentId,
    /// Projected message body and complete metrics.
    #[serde(flatten)]
    text: TextProjection,
}

/// Projects assistant prose and displayable reasoning only.
pub(super) fn project_provider_item(
    item: &ContextItem,
    prompt_id: &tau_proto::AgentPromptId,
    mode: super::super::AgentTraceMode,
) -> Option<SemanticProjection> {
    match item {
        ContextItem::Message(message) if message.role == ContextRole::Assistant => {
            let text = message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => {
                        text.as_str()
                    }
                })
                .collect::<String>();
            Some(SemanticProjection {
                record: SemanticRecord::AssistantMessage(AssistantMessageRecord {
                    record_type: "assistant_message",
                    agent_prompt_id: prompt_id.clone(),
                    phase: message.phase,
                    text: TextProjection::new(&text, mode),
                }),
                rank: RecordRank::AssistantMessage,
            })
        }
        ContextItem::ReasoningText(reasoning) => Some(SemanticProjection {
            record: SemanticRecord::AssistantReasoning(AssistantReasoningRecord {
                record_type: "assistant_reasoning",
                agent_prompt_id: prompt_id.clone(),
                reasoning_kind: reasoning.kind,
                text: TextProjection::new(&reasoning.text, mode),
            }),
            rank: RecordRank::AssistantReasoning,
        }),
        _ => None,
    }
}

/// Projects explicit sender- and recipient-side message facts only.
pub(super) fn project_message_event(
    event: &Event,
    mode: super::super::AgentTraceMode,
) -> Option<SemanticProjection> {
    match event {
        Event::AgentMessageSent(sent) if sent.kind == AgentMessageKind::Message => {
            let recipient = match &sent.recipient {
                AgentMessageRecipient::Agent { agent_id } => MessageRecipientRecord::Agent {
                    recipient_id: agent_id.clone(),
                },
                AgentMessageRecipient::ExternalAgent {
                    session_id,
                    agent_id,
                } => MessageRecipientRecord::ExternalAgent {
                    recipient_id: agent_id.clone(),
                    recipient_session_id: session_id.clone(),
                },
                AgentMessageRecipient::User => MessageRecipientRecord::User,
            };
            Some(SemanticProjection {
                record: SemanticRecord::MessageSent(MessageSentRecord {
                    record_type: "message_sent",
                    message_id: sent.message_id.clone(),
                    sender_id: sent.sender_id.clone(),
                    recipient,
                    text: TextProjection::new(&sent.message, mode),
                }),
                rank: RecordRank::MessageSent,
            })
        }
        Event::AgentMessageReceived(received) if received.kind == AgentMessageKind::Message => {
            Some(SemanticProjection {
                record: SemanticRecord::MessageReceived(MessageReceivedRecord {
                    record_type: "message_received",
                    message_id: received.message_id.clone(),
                    sender_id: received.sender_id.clone(),
                    sender_session_id: received.sender_session_id.clone(),
                    recipient_id: received.recipient_id.clone(),
                    text: TextProjection::new(&received.message, mode),
                }),
                rank: RecordRank::MessageReceived,
            })
        }
        _ => None,
    }
}

/// Typed TOON semantic payload with unsafe fields structurally framed.
#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum ToonSemanticRecord {
    /// TOON-safe assistant prose.
    AssistantMessage(ToonAssistantMessageRecord),
    /// TOON-safe displayable reasoning.
    AssistantReasoning(ToonAssistantReasoningRecord),
    /// TOON-safe sender-side message.
    MessageSent(ToonMessageSentRecord),
    /// TOON-safe recipient-side message.
    MessageReceived(ToonMessageReceivedRecord),
}

/// Direct or Base64 semantic text plus unchanged complete metrics.
#[derive(Serialize)]
pub(super) struct ToonTextProjection {
    /// Complete canonical UTF-8 byte count.
    text_bytes: usize,
    /// Complete canonical line count.
    text_lines: usize,
    /// Direct or framed selected text.
    #[serde(flatten)]
    text: ToonText,
    /// Whether selected text is complete.
    text_complete: bool,
}

/// Direct or Base64 selected semantic text.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonText {
    /// Grammar-safe direct text.
    Direct {
        /// Selected semantic text.
        text: String,
    },
    /// Unsafe text framed as Base64 UTF-8 bytes.
    Base64 {
        /// Standard padded Base64 text bytes.
        text_base64: String,
    },
}

/// Direct or Base64 message identity.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonMessageId {
    /// Grammar-safe direct message identity.
    Direct {
        /// Stable logical message identity.
        message_id: tau_proto::AgentMessageId,
    },
    /// Unsafe identity framed as Base64 UTF-8 bytes.
    Base64 {
        /// Standard padded Base64 identity bytes.
        message_id_base64: String,
    },
}

/// TOON-safe assistant prose occurrence.
#[derive(Serialize)]
pub(super) struct ToonAssistantMessageRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Canonical prompt identity.
    agent_prompt_id: tau_proto::AgentPromptId,
    /// Optional provider phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<tau_proto::MessagePhase>,
    /// TOON-safe projected text.
    #[serde(flatten)]
    text: ToonTextProjection,
}

/// TOON-safe reasoning occurrence.
#[derive(Serialize)]
pub(super) struct ToonAssistantReasoningRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Canonical prompt identity.
    agent_prompt_id: tau_proto::AgentPromptId,
    /// Exact reasoning kind.
    reasoning_kind: tau_proto::ReasoningTextKind,
    /// TOON-safe projected text.
    #[serde(flatten)]
    text: ToonTextProjection,
}

/// TOON-safe sender-side message occurrence.
#[derive(Serialize)]
pub(super) struct ToonMessageSentRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Direct or framed message identity.
    #[serde(flatten)]
    message_id: ToonMessageId,
    /// Canonical sender identity.
    sender_id: tau_proto::AgentId,
    /// Typed recipient identity.
    #[serde(flatten)]
    recipient: MessageRecipientRecord,
    /// TOON-safe projected message body.
    #[serde(flatten)]
    text: ToonTextProjection,
}

/// TOON-safe recipient-side message occurrence.
#[derive(Serialize)]
pub(super) struct ToonMessageReceivedRecord {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Direct or framed message identity.
    #[serde(flatten)]
    message_id: ToonMessageId,
    /// Canonical sender identity.
    sender_id: tau_proto::AgentId,
    /// Optional historical sender session.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_session_id: Option<tau_proto::SessionId>,
    /// Canonical recipient identity.
    recipient_id: tau_proto::AgentId,
    /// TOON-safe projected message body.
    #[serde(flatten)]
    text: ToonTextProjection,
}

impl From<TextProjection> for ToonTextProjection {
    fn from(value: TextProjection) -> Self {
        let text = if contains_unsafe_string(&value.text) {
            ToonText::Base64 {
                text_base64: encode_bytes(value.text.as_bytes()),
            }
        } else {
            ToonText::Direct { text: value.text }
        };
        Self {
            text_bytes: value.text_bytes,
            text_lines: value.text_lines,
            text,
            text_complete: value.text_complete,
        }
    }
}

impl From<tau_proto::AgentMessageId> for ToonMessageId {
    fn from(value: tau_proto::AgentMessageId) -> Self {
        if contains_unsafe_string(value.as_str()) {
            Self::Base64 {
                message_id_base64: encode_bytes(value.as_str().as_bytes()),
            }
        } else {
            Self::Direct { message_id: value }
        }
    }
}

impl From<SemanticRecord> for ToonSemanticRecord {
    fn from(value: SemanticRecord) -> Self {
        match value {
            SemanticRecord::AssistantMessage(value) => {
                Self::AssistantMessage(ToonAssistantMessageRecord {
                    record_type: value.record_type,
                    agent_prompt_id: value.agent_prompt_id,
                    phase: value.phase,
                    text: value.text.into(),
                })
            }
            SemanticRecord::AssistantReasoning(value) => {
                Self::AssistantReasoning(ToonAssistantReasoningRecord {
                    record_type: value.record_type,
                    agent_prompt_id: value.agent_prompt_id,
                    reasoning_kind: value.reasoning_kind,
                    text: value.text.into(),
                })
            }
            SemanticRecord::MessageSent(value) => Self::MessageSent(ToonMessageSentRecord {
                record_type: value.record_type,
                message_id: value.message_id.into(),
                sender_id: value.sender_id,
                recipient: value.recipient,
                text: value.text.into(),
            }),
            SemanticRecord::MessageReceived(value) => {
                Self::MessageReceived(ToonMessageReceivedRecord {
                    record_type: value.record_type,
                    message_id: value.message_id.into(),
                    sender_id: value.sender_id,
                    sender_session_id: value.sender_session_id,
                    recipient_id: value.recipient_id,
                    text: value.text.into(),
                })
            }
        }
    }
}

/// Returns whether TOON cannot safely escape one string directly.
fn contains_unsafe_string(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

/// Encodes UTF-8 bytes as standard padded Base64.
fn encode_bytes(bytes: &[u8]) -> String {
    path_base64_engine::general_purpose::STANDARD.encode(bytes)
}
