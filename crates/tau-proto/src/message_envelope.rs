//! Transport-neutral external and Tau message protocol schema.

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, ContentPart, ContextRole, ExtensionName, MessageId, MessageItem, SessionId,
    ToolCallId, ToolName, UnixMicros,
};

/// Harness-stamped transport identity for one message occurrence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageTransportRef {
    /// Stable transport family such as `slack`, `telegram`, `xmpp`, or `tau`.
    pub name: String,
    /// Authenticated extension instance, absent for harness-owned Tau
    /// transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<ExtensionName>,
}

/// One endpoint participating in a canonical message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageEndpoint {
    /// Tau agent endpoint.
    Agent {
        /// Session containing the agent when relevant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        /// Durable agent id.
        agent_id: AgentId,
        /// Presentation-only display name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// Authenticated local human UI.
    User,
    /// External transport actor.
    External {
        /// Transport-stable actor id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stable_id: Option<String>,
        /// Presentation-only actor label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        /// Transport-reported actor class.
        actor_kind: ExternalActorKind,
    },
}

/// External actor classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalActorKind {
    /// Human account.
    Human,
    /// Bot account.
    Bot,
    /// Service/integration actor.
    Service,
    /// Actor class was not established.
    Unknown,
}

/// Transport conversation metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageConversation {
    /// Conversation class.
    pub kind: ConversationKind,
    /// Transport-stable conversation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    /// Presentation-only conversation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional thread relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<MessageThread>,
    /// Optional immediate reply target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageRef>,
}

/// Conversation classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    /// One-to-one direct conversation.
    Direct,
    /// Named/shared channel.
    Channel,
    /// Multi-user room.
    Room,
    /// Group conversation.
    Group,
    /// Transport-specific unknown class.
    Unknown,
}

/// Thread metadata scoped to a conversation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageThread {
    /// Transport-stable thread id.
    pub stable_id: String,
    /// Optional canonical/native root reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<MessageRef>,
}

/// Reference to an earlier canonical or native message.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageRef {
    /// Canonical Tau message id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Transport-native message id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_message_id: Option<String>,
}

/// Immutable operation represented by one message occurrence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageOperation {
    /// Create a new payload.
    Create { payload: MessagePayload },
    /// Edit an earlier message without rewriting it.
    Edit {
        /// Edited target.
        target: MessageRef,
        /// Replacement payload.
        payload: MessagePayload,
    },
    /// Delete an earlier message without rewriting it.
    Delete {
        /// Deleted target.
        target: MessageRef,
    },
    /// Add or remove a reaction.
    Reaction {
        /// Reacted-to target.
        target: MessageRef,
        /// Reaction action.
        action: ReactionAction,
        /// Canonical reaction metadata.
        reaction: MessageReaction,
    },
}

/// Message payload supported by schema version one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessagePayload {
    /// Plain text payload.
    Text {
        /// Exact text.
        text: String,
        /// Declared text format.
        format: TextFormat,
    },
}

/// Payload text format.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// Unformatted Unicode text.
    Plain,
}

/// Reaction mutation action.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionAction {
    /// Add reaction.
    Add,
    /// Remove reaction.
    Remove,
}

/// Canonical reaction name and optional display glyph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageReaction {
    /// Stable transport-neutral or transport-native name.
    pub name: String,
    /// Optional display glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

/// Native identity and deduplication metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalMessageIdentity {
    /// Transport occurrence/event id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Transport logical message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Transport revision id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// Stable transport-instance-scoped dedup key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
}

/// Source ordering metadata when the transport exposes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageOrdering {
    /// Monotonic sequence scoped to the source route.
    pub source_sequence: u64,
}

/// Content and identity trust metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageTrust {
    /// Content authority classification.
    pub content: MessageContentTrust,
    /// Sender identity assurance.
    pub identity: SenderIdentityAssurance,
    /// Sender routing-policy status.
    pub policy: SenderPolicyStatus,
}

/// Message content trust class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageContentTrust {
    /// Authenticated Tau agent content.
    AuthenticatedTauAgent,
    /// Untrusted external transport content.
    UntrustedExternal,
    /// Harness-authored internal content.
    HarnessInternal,
}

/// Sender identity assurance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderIdentityAssurance {
    /// Transport account id was verified.
    VerifiedAccount,
    /// Sender was verified through room membership.
    RoomMembership,
    /// Identity is presentation-only.
    DisplayOnly,
    /// Harness authenticated a Tau agent.
    AuthenticatedTauAgent,
    /// No identity assurance.
    Unknown,
}

/// Sender policy acceptance class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderPolicyStatus {
    /// Explicitly allowlisted sender.
    Allowlisted,
    /// Sender admitted by lax conversation policy.
    LaxPermitted,
    /// Harness-internal sender.
    Internal,
}

/// Non-secret hint for selecting a live extension-owned reply route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageReplyPath {
    /// Extension-owned reply tool.
    pub tool_name: ToolName,
    /// Selector shape exposed to the model.
    pub selector: ReplySelector,
    /// Route lifetime.
    pub lifetime: ReplyPathLifetime,
}

/// Reply selector accepted by a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplySelector {
    /// Canonical `reply_to` message id.
    ReplyToMessage,
}

/// Reply route lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyPathLifetime {
    /// Valid only in the active harness session.
    ActiveSession,
}

/// Canonical transport-neutral message occurrence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageEnvelope {
    /// Harness-minted canonical occurrence id.
    pub message_id: MessageId,
    /// Harness-stamped transport identity.
    pub transport: MessageTransportRef,
    /// Source endpoint.
    pub source: MessageEndpoint,
    /// Destination endpoint.
    pub destination: MessageEndpoint,
    /// Optional conversation relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Immutable operation.
    pub operation: MessageOperation,
    /// Trust classification.
    pub trust: MessageTrust,
    /// Optional native identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_identity: Option<ExternalMessageIdentity>,
    /// Optional source sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordering: Option<MessageOrdering>,
    /// Transport-claimed occurrence time; never ordering authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<UnixMicros>,
    /// Optional non-secret reply-path hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_path: Option<MessageReplyPath>,
}

/// Direction of a typed provider/UI message projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    /// Incoming to the owning agent.
    Incoming,
    /// Outgoing from the owning agent.
    Outgoing,
}

/// Harness-derived provider presentation policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageModelPresentation {
    /// Safe transport label.
    pub transport_label: String,
    /// Safe source label.
    pub source_label: String,
    /// Safe conversation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_label: Option<String>,
}

/// Typed provider-context item for one canonical envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageEnvelopeItem {
    /// Projection direction.
    pub direction: MessageDirection,
    /// Canonical envelope.
    pub envelope: MessageEnvelope,
    /// Harness-derived model presentation.
    pub model_presentation: MessageModelPresentation,
}

impl MessageEnvelopeItem {
    /// Lower this typed envelope into one escaped provider-compatible message.
    #[must_use]
    pub fn to_provider_message(&self) -> MessageItem {
        MessageItem {
            role: match self.direction {
                MessageDirection::Incoming => ContextRole::User,
                MessageDirection::Outgoing => ContextRole::Assistant,
            },
            content: vec![ContentPart::Text {
                text: self.render_provider_text(),
            }],
            phase: None,
            responses_raw_json: None,
        }
    }

    /// Render deterministic provider text while preserving payload boundaries.
    #[must_use]
    pub fn render_provider_text(&self) -> String {
        let direction = match self.direction {
            MessageDirection::Incoming => "incoming",
            MessageDirection::Outgoing => "outgoing",
        };
        let payload = match &self.envelope.operation {
            MessageOperation::Create {
                payload: MessagePayload::Text { text, .. },
            }
            | MessageOperation::Edit {
                payload: MessagePayload::Text { text, .. },
                ..
            } => text.as_str(),
            MessageOperation::Delete { .. } => "[message deleted]",
            MessageOperation::Reaction {
                action, reaction, ..
            } => {
                return format!(
                    "<tau_message version=\"1\" direction=\"{direction}\">\ntransport: {}\nmessage_id: {}\ncontent_trust: {}\noperation: reaction {} {}\n</tau_message>",
                    xml_escape(&self.model_presentation.transport_label),
                    xml_escape(self.envelope.message_id.as_str()),
                    content_trust_name(self.envelope.trust.content),
                    reaction_action_name(*action),
                    xml_escape(&reaction.name),
                );
            }
        };
        let reply = self
            .envelope
            .reply_path
            .as_ref()
            .map(|path| {
                format!(
                    "\nreply: {}(reply_to=\"{}\")",
                    xml_escape(path.tool_name.as_str()),
                    xml_escape(self.envelope.message_id.as_str())
                )
            })
            .unwrap_or_default();
        let conversation = self
            .model_presentation
            .conversation_label
            .as_ref()
            .map(|label| format!("\nconversation: {}", xml_escape(label)))
            .unwrap_or_default();
        format!(
            "<tau_message version=\"1\" direction=\"{direction}\">\ntransport: {}\nmessage_id: {}\nfrom: {}{conversation}\ncontent_trust: {}\nsender_identity: {}\nsender_policy: {}{reply}\n<payload type=\"text\">\n{}\n</payload>\n</tau_message>",
            xml_escape(&self.model_presentation.transport_label),
            xml_escape(self.envelope.message_id.as_str()),
            xml_escape(&self.model_presentation.source_label),
            content_trust_name(self.envelope.trust.content),
            identity_assurance_name(self.envelope.trust.identity),
            policy_status_name(self.envelope.trust.policy),
            xml_escape(payload),
        )
    }
}

fn content_trust_name(value: MessageContentTrust) -> &'static str {
    match value {
        MessageContentTrust::AuthenticatedTauAgent => "authenticated_tau_agent",
        MessageContentTrust::UntrustedExternal => "untrusted_external",
        MessageContentTrust::HarnessInternal => "harness_internal",
    }
}

fn identity_assurance_name(value: SenderIdentityAssurance) -> &'static str {
    match value {
        SenderIdentityAssurance::VerifiedAccount => "verified_account",
        SenderIdentityAssurance::RoomMembership => "room_membership",
        SenderIdentityAssurance::DisplayOnly => "display_only",
        SenderIdentityAssurance::AuthenticatedTauAgent => "authenticated_tau_agent",
        SenderIdentityAssurance::Unknown => "unknown",
    }
}

fn policy_status_name(value: SenderPolicyStatus) -> &'static str {
    match value {
        SenderPolicyStatus::Allowlisted => "allowlisted",
        SenderPolicyStatus::LaxPermitted => "lax_permitted",
        SenderPolicyStatus::Internal => "internal",
    }
}

fn reaction_action_name(value: ReactionAction) -> &'static str {
    match value {
        ReactionAction::Add => "add",
        ReactionAction::Remove => "remove",
    }
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            ch if ch.is_control() && !matches!(ch, '\n' | '\t') => {
                escaped.push_str(&format!("\\u{{{:x}}}", ch as u32));
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

/// Prompt submission provenance stamped by the harness boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSubmissionSource {
    /// Authenticated interactive UI.
    HumanUi,
    /// Extension-originated input.
    Extension {
        /// Authenticated extension instance name.
        name: ExtensionName,
    },
    /// Harness-internal input.
    HarnessInternal,
    /// Legacy record without explicit provenance.
    #[default]
    Legacy,
}

/// Transport acceptance strength for an outgoing occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTransportAcceptance {
    /// Accepted by the transport API/client.
    SubmittedToTransport,
    /// Accepted by the remote server.
    AcceptedByServer,
}

/// Extension-provided normalized ingress draft before harness stamping.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportMessageDraft {
    /// Requested transport family. The harness stamps the instance.
    pub transport_name: String,
    /// External endpoint: ingress source or egress destination.
    pub external_endpoint: MessageEndpoint,
    /// Optional conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<MessageConversation>,
    /// Immutable operation.
    pub operation: MessageOperation,
    /// Sender identity assurance.
    pub identity_assurance: SenderIdentityAssurance,
    /// Sender policy status.
    pub policy_status: SenderPolicyStatus,
    /// Optional native identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_identity: Option<ExternalMessageIdentity>,
    /// Optional ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordering: Option<MessageOrdering>,
    /// Optional claimed occurrence time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<UnixMicros>,
    /// Extension-owned reply tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_tool: Option<ToolName>,
}

/// Dedicated request to durably accept one transport ingress occurrence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportMessageIngressRequest {
    /// Caller correlation id.
    pub request_id: String,
    /// Target loaded agent.
    pub target_agent_id: AgentId,
    /// Normalized external draft.
    pub draft: TransportMessageDraft,
}

/// Source-bound declaration that an extension instance owns one transport and
/// optional reply tool for the active session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RegisterTransportCapabilityRequest {
    /// Caller correlation id.
    pub request_id: String,
    /// Transport family this extension will submit.
    pub transport_name: String,
    /// Reply tool owned by the same connection, when replies are supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_tool: Option<ToolName>,
}

/// Result of source-bound transport capability registration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RegisterTransportCapabilityResult {
    /// Caller correlation id.
    pub request_id: String,
    /// Whether the capability is active.
    pub accepted: bool,
    /// Bounded failure diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Idempotent ingress outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMessageIngressOutcome {
    /// New fact committed.
    Accepted,
    /// Identical committed fact already existed.
    Duplicate,
}

/// Result of a transport ingress request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportMessageIngressResult {
    /// Caller correlation id.
    pub request_id: String,
    /// Canonical id when accepted/duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Success outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TransportMessageIngressOutcome>,
    /// Bounded error code/message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Successful transport egress report awaiting durable completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompleteTransportSendRequest {
    /// Caller correlation id.
    pub request_id: String,
    /// Tool call being completed.
    pub call_id: ToolCallId,
    /// Owning agent.
    pub agent_id: AgentId,
    /// Canonical incoming message selected as reply route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<MessageId>,
    /// Actual accepted outgoing draft.
    pub draft: TransportMessageDraft,
    /// Transport acceptance strength.
    pub acceptance: MessageTransportAcceptance,
    /// Terminal successful tool result committed after the outgoing fact.
    pub tool_result: crate::ToolResult,
}

/// Result of successful-send completion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompleteTransportSendResult {
    /// Caller correlation id.
    pub request_id: String,
    /// Canonical outgoing id on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Whether completion succeeded.
    pub accepted: bool,
    /// Bounded failure diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
