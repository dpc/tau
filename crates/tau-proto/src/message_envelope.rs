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
        /// Presentation-only alias with explicit local authority.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_alias: Option<ExternalIdentityAlias>,
        /// Transport-reported actor class.
        actor_kind: ExternalActorKind,
    },
}

/// Presentation-only alias for an external identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalIdentityAlias {
    /// Stable bounded alias.
    pub value: String,
    /// Authority that assigned the alias.
    pub authority: ExternalIdentityAliasAuthority,
}

/// Closed provenance for an external identity alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalIdentityAliasAuthority {
    /// The local operator bound the alias to an exact transport identity.
    OperatorConfigured,
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

/// Temporary legacy harness-managed transport conversation metadata.
///
/// New extension-published facts use [`crate::MessageConversation`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyMessageConversation {
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

/// Native transport identity metadata.
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
    pub conversation: Option<LegacyMessageConversation>,
    /// Immutable operation.
    pub operation: MessageOperation,
    /// Informational, non-authoritative fact that normalized Create/Edit text
    /// addressed this transport instance's authenticated receiving identity.
    ///
    /// Absent legacy data defaults to false. This must remain false for
    /// textless operations and grants no routing, instruction, or transport
    /// capability.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transport_identity_mentioned: bool,
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
    /// Harness-authenticated transport instance that scopes source aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_instance_label: Option<String>,
    /// Stable presentation-only source alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_alias: Option<ExternalIdentityAlias>,
    /// Safe conversation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_label: Option<String>,
    /// Prompt-local model-visible reply tool, present only while its route and
    /// effective tool snapshot are both live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_send_tool: Option<ToolName>,
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
        let (payload, operation, target, reaction, include_reply) = match &self.envelope.operation {
            MessageOperation::Create {
                payload: MessagePayload::Text { text, .. },
            } => (Some(text.as_str()), None, None, None, true),
            MessageOperation::Edit {
                target,
                payload: MessagePayload::Text { text, .. },
            } => (Some(text.as_str()), Some("edit"), Some(target), None, true),
            MessageOperation::Delete { target } => (None, Some("delete"), Some(target), None, true),
            MessageOperation::Reaction {
                target,
                action,
                reaction,
            } => (
                None,
                Some(match action {
                    ReactionAction::Add => "reaction_add",
                    ReactionAction::Remove => "reaction_remove",
                }),
                Some(target),
                Some(reaction.name.as_str()),
                false,
            ),
        };
        let mut rendered = String::from("<tau_message");
        push_xml_attribute(
            &mut rendered,
            "transport",
            &self.model_presentation.transport_label,
        );
        if let Some(instance) = &self.model_presentation.transport_instance_label {
            push_xml_attribute(&mut rendered, "transport_instance", instance);
        }
        push_xml_attribute(
            &mut rendered,
            "message_id",
            self.envelope.message_id.as_str(),
        );
        push_xml_attribute(
            &mut rendered,
            "sender",
            &self.model_presentation.source_label,
        );
        if self.envelope.transport_identity_mentioned
            && matches!(
                &self.envelope.operation,
                MessageOperation::Create { .. } | MessageOperation::Edit { .. }
            )
        {
            push_xml_attribute(&mut rendered, "transport_identity_mentioned", "true");
        }
        if let Some(alias) = &self.model_presentation.source_alias {
            push_xml_attribute(&mut rendered, "sender_alias", &alias.value);
            push_xml_attribute(
                &mut rendered,
                "sender_alias_authority",
                match alias.authority {
                    ExternalIdentityAliasAuthority::OperatorConfigured => "operator_configured",
                },
            );
        }
        if let Some(conversation) = &self.model_presentation.conversation_label {
            push_xml_attribute(&mut rendered, "conversation", conversation);
        }
        push_xml_attribute(
            &mut rendered,
            "origin",
            origin_name(self.envelope.trust.content),
        );
        if let Some(allowlisted) = sender_allowlisted(self.envelope.trust.policy) {
            push_xml_attribute(&mut rendered, "sender_allowlisted", allowlisted);
        }
        if let Some(operation) = operation {
            push_xml_attribute(&mut rendered, "operation", operation);
        }
        if let Some(target) = target {
            push_xml_attribute(&mut rendered, "target", &message_ref_label(target));
        }
        if let Some(reaction) = reaction {
            push_xml_attribute(&mut rendered, "reaction", reaction);
        }
        if include_reply && let Some(tool) = &self.model_presentation.live_send_tool {
            push_xml_attribute(&mut rendered, "reply", tool.as_str());
        }
        if let Some(payload) = payload {
            rendered.push('>');
            rendered.push_str(&xml_text_escape(payload));
            rendered.push_str("</tau_message>");
        } else {
            rendered.push_str("/>");
        }
        rendered
    }
}

fn push_xml_attribute(output: &mut String, name: &str, value: &str) {
    output.push(' ');
    output.push_str(name);
    output.push_str("=\"");
    output.push_str(&xml_attribute_escape(value));
    output.push('"');
}

fn message_ref_label(reference: &MessageRef) -> String {
    reference
        .message_id
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| reference.external_message_id.clone())
        .unwrap_or_else(|| "unresolved".to_owned())
}

fn origin_name(value: MessageContentTrust) -> &'static str {
    match value {
        MessageContentTrust::AuthenticatedTauAgent => "agent",
        MessageContentTrust::UntrustedExternal => "external",
        MessageContentTrust::HarnessInternal => "internal",
    }
}

fn sender_allowlisted(value: SenderPolicyStatus) -> Option<&'static str> {
    match value {
        SenderPolicyStatus::Allowlisted => Some("true"),
        SenderPolicyStatus::LaxPermitted => Some("false"),
        SenderPolicyStatus::Internal => None,
    }
}

/// Return whether untrusted metadata must be rendered as a visible escape.
///
/// This includes controls, bidi/zero-width structure, Unicode default
/// ignorables used to spoof visible labels, variation selectors, and
/// noncharacters.
#[must_use]
pub fn requires_visible_escape(character: char) -> bool {
    let scalar = character as u32;
    character.is_control()
        || matches!(
            scalar,
            0x00AD
                | 0x034F
                | 0x061C
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x206F
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFF0..=0xFFF8
                | 0xFFA0
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
                | 0xFDD0..=0xFDEF
        )
        || scalar & 0xFFFF == 0xFFFE
        || scalar & 0xFFFF == 0xFFFF
}

/// Render untrusted metadata with structural Unicode made explicit.
#[must_use]
pub fn visible_escape_metadata(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if requires_visible_escape(character) {
            push_visible_escape(&mut escaped, character);
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn push_visible_escape(output: &mut String, character: char) {
    use std::fmt::Write as _;
    let _ = write!(output, "\\u{{{:04X}}}", character as u32);
}

fn xml_attribute_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character if requires_visible_escape(character) => {
                push_visible_escape(&mut escaped, character)
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn xml_text_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\n' | '\t' => escaped.push(character),
            character if requires_visible_escape(character) => {
                push_visible_escape(&mut escaped, character)
            }
            character => escaped.push(character),
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
    pub conversation: Option<LegacyMessageConversation>,
    /// Immutable operation.
    pub operation: MessageOperation,
    /// Informational, non-authoritative fact that normalized Create/Edit text
    /// addressed the transport's authenticated receiving identity.
    ///
    /// Absent data defaults to false. This must remain false for textless
    /// operations and grants no routing, instruction, or transport capability.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transport_identity_mentioned: bool,
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
    pub send_tool: Option<ToolName>,
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
    pub send_tool: Option<ToolName>,
    /// Exact operator-configured proactive routes owned by this connection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub send_destinations: Vec<TransportSendDestinationCapability>,
}

/// One exact model-facing alias to a transport-private destination.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportSendDestinationCapability {
    /// Stable model-facing alias.
    pub alias: String,
    /// Exact external endpoint expected in a completion.
    pub external_endpoint: MessageEndpoint,
    /// Exact native conversation and optional fixed thread.
    pub conversation: LegacyMessageConversation,
}

/// Authority used for one transport send completion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportSendAuthorization {
    /// Live source-bound reply route.
    Reply {
        /// Canonical incoming message.
        message_id: MessageId,
    },
    /// Operator-configured proactive route.
    ConfiguredDestination {
        /// Stable configured alias.
        alias: String,
    },
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

/// Successful ingress outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMessageIngressOutcome {
    /// New fact committed.
    Accepted,
}

/// Result of a transport ingress request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportMessageIngressResult {
    /// Caller correlation id.
    pub request_id: String,
    /// Canonical id when accepted.
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
    /// Tagged authority selected for this send.
    pub authorization: TransportSendAuthorization,
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
