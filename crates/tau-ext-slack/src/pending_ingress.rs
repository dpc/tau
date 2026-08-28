//! Canonical-confirmation state for Slack ingress reports.

use super::*;
use crate::{SlackAgentGeneration, SlackConfigGeneration, SlackIngressEpoch};

/// Extension-data map key carrying one Slack report occurrence identity.
const SLACK_REPORT_ID_KEY: &str = "slack_report_id";

/// Typed opaque identity shared by a transient report and its canonical fact.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct SlackReportId(String);

impl SlackReportId {
    /// Derive a stable opaque report ID from Slack's native occurrence
    /// identity.
    pub(super) fn from_occurrence(occurrence_key: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tau-ext-slack/report-id/v1\0");
        hasher.update(occurrence_key.as_bytes());
        Self(format!("slack-report:{}", hasher.finalize().to_hex()))
    }

    /// Parse Slack's exact private report correlation field from a canonical
    /// fact.
    pub(super) fn from_extension_data(data: &MessageExtensionData) -> Option<Self> {
        let CborValue::Map(fields) = data.value() else {
            return None;
        };
        fields.iter().find_map(|(key, value)| {
            if !matches!(key, CborValue::Text(key) if key == SLACK_REPORT_ID_KEY) {
                return None;
            }
            match value {
                CborValue::Text(value) => Some(Self(value.clone())),
                _ => None,
            }
        })
    }

    /// Encode the report ID preserved unchanged by canonicalization.
    pub(super) fn extension_data(&self) -> MessageExtensionData {
        MessageExtensionData::new(CborValue::Map(vec![(
            CborValue::Text(SLACK_REPORT_ID_KEY.to_owned()),
            CborValue::Text(self.0.clone()),
        )]))
        .expect("fixed Slack report correlation data is bounded")
    }
}

/// Canonical message event expected for one submitted Slack ingress report.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PendingIngressKind {
    /// Newly delivered external message.
    Delivered,
    /// External message edit.
    Edited,
    /// External message deletion.
    Deleted,
    /// External reaction addition.
    ReactionAdded,
    /// External reaction removal.
    ReactionRemoved,
}

/// Source authority deferred until a message report becomes canonical.
pub(super) struct PendingMessageAuthority {
    /// Exact native message identity used for edit/delete lookup.
    pub(super) original_key: Option<PostedMessageKey>,
    /// Exact source-bound reply route.
    pub(super) reply_route: ReplyRoute,
    /// Native timestamp used for reaction routing.
    pub(super) reaction_message_ts: Option<String>,
    /// Exact source conversation.
    pub(super) conversation: SlackConversation,
    /// Verified source user.
    pub(super) user_id: String,
    /// Authenticated installation at submission.
    pub(super) installation_team_id: String,
}

/// One report retained until its exact canonical fact returns on the live
/// downpath.
pub(super) struct PendingIngress {
    /// Expected canonical event family.
    pub(super) kind: PendingIngressKind,
    /// Expected raw target agent.
    pub(super) agent_id: AgentId,
    /// Expected base or referenced message identity.
    pub(super) message_id: MessageFactId,
    /// Ingress lifecycle epoch captured at submission.
    pub(super) ingress_epoch: SlackIngressEpoch,
    /// Configuration generation captured at submission.
    pub(super) config_generation: SlackConfigGeneration,
    /// Agent-routing generation captured at submission.
    pub(super) agent_generation: SlackAgentGeneration,
    /// Deferred delivered/edit message source authority.
    pub(super) message_authority: Option<PendingMessageAuthority>,
    /// Exact report replayed if Slack redelivers before canonical confirmation.
    pub(super) report: Event,
    /// Pre-ACK admission slot held until canonical confirmation.
    pub(super) _permit: Option<OutstandingPermit<AdmissionWork>>,
}

/// Result of checking one native occurrence against process-local dedupe and
/// pending report state.
pub(super) enum OccurrenceDisposition {
    /// First observation may continue through admission.
    New,
    /// The exact retained report must be replayed without recomputing routing.
    Pending(SlackReportId),
    /// A canonically confirmed occurrence is suppressed.
    ConfirmedDuplicate,
}

impl State {
    /// Release pending canonical reports owned by one retired agent.
    pub(super) fn remove_agent_pending_ingress(&mut self, agent_id: &AgentId) {
        self.ingress
            .pending_ingress
            .retain(|_, pending| &pending.agent_id != agent_id);
    }

    /// Retire one exact pending ingress report and install any deferred source
    /// authority only after its canonical fact returns.
    pub(super) fn acknowledge_canonical_ingress(
        &mut self,
        kind: PendingIngressKind,
        publisher: &tau_proto::MessagePublisherId,
        agent_id: &MessageAgentTarget,
        message_id: &MessageFactId,
        extension_data: &MessageExtensionData,
    ) {
        if self
            .configuration
            .instance_name
            .as_ref()
            .is_none_or(|expected| expected.as_str() != publisher.as_str())
        {
            return;
        }
        let Some(report_id) = SlackReportId::from_extension_data(extension_data) else {
            return;
        };
        let matches = self
            .ingress
            .pending_ingress
            .get(&report_id)
            .is_some_and(|pending| {
                pending.kind == kind
                    && pending.agent_id.as_str() == agent_id.as_str()
                    && &pending.message_id == message_id
            });
        if !matches {
            return;
        }
        let Some(pending) = self.ingress.pending_ingress.remove(&report_id) else {
            return;
        };
        let Some(authority) = pending.message_authority else {
            return;
        };
        let current = self.ingress.ingress_epoch == pending.ingress_epoch
            && self.configuration.config_generation == pending.config_generation
            && self.agents.agent_generation == pending.agent_generation
            && self.agents.registered_agents.contains(&pending.agent_id)
            && self.socket.installation_team_id.as_deref()
                == Some(authority.installation_team_id.as_str())
            && self.configuration.config.as_ref().is_some_and(|cfg| {
                is_route_authorized(self, cfg, &authority.conversation, &authority.user_id)
            });
        if !current {
            return;
        }
        if let Some(original_key) = authority.original_key {
            self.insert_incoming_message(
                original_key,
                IncomingMessageOwner {
                    agent_id: pending.agent_id.clone(),
                    message_id: pending.message_id.clone(),
                    conversation: authority.conversation.clone(),
                    user_id: authority.user_id.clone(),
                },
            );
        }
        self.insert_reply_route(pending.message_id.clone(), authority.reply_route);
        if let Some(message_ts) = authority.reaction_message_ts {
            let _ = self.ingress.reactions.insert_target(
                pending.message_id.clone(),
                ReactionTarget {
                    agent_id: pending.agent_id,
                    conversation: authority.conversation,
                    message_ts,
                    installation_team_id: authority.installation_team_id,
                    authority: ReactionAuthority::Source {
                        message_id: pending.message_id,
                        user_id: authority.user_id,
                    },
                },
            );
        }
    }
}
