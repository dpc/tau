//! Generic passive append admission and publication outcomes.

use tau_proto::{
    AgentPassiveContextAppended, ExtPassiveContextAppendRequest, ExtPassiveContextAppendResult,
    PassiveContextAppendOutcome,
};

use super::*;

impl Harness {
    /// Turn one committed, authenticated request into a separate canonical
    /// fact.
    pub(super) fn process_committed_passive_context_request(
        &mut self,
        peer: &interception::PeerPublicationContext,
        request: &ExtPassiveContextAppendRequest,
    ) {
        let Some(extension) = peer.extension.as_ref() else {
            return;
        };
        let current = self
            .extensions
            .entries
            .get(&extension.source)
            .is_some_and(|entry| {
                extension.admission.session_id == self.session_runtime.current_session_id
                    && extension.admission.session_generation
                        == self.session_runtime.current_session_generation
                    && entry.instance_id == extension.instance_id
                    && entry.name == extension.publisher
                    && entry.state != ExtensionState::Disconnected
            });
        let mut appended = AgentPassiveContextAppended {
            session_id: request.session_id.clone(),
            agent_id: request.agent_id.clone(),
            publisher: extension.publisher.clone(),
            operation_id: request.operation_id.clone(),
            text: String::new(),
        };
        if !current
            || request.session_id != self.session_runtime.current_session_id
            || request.operation_id.is_empty()
            || request.operation_id.len() > 256
            || request.body.len() > tau_proto::PASSIVE_CONTEXT_BODY_LIMIT
        {
            self.passive_context_append_result(
                &appended,
                PassiveContextAppendOutcome::Failed {
                    reason: "invalid or stale passive context append request".to_owned(),
                },
            );
            return;
        }
        appended.text = tau_proto::EXTENSION_OUTPUT_PAYLOAD_ENVELOPE
            .render_attributed(
                &[
                    ("publisher", appended.publisher.to_string()),
                    ("operation_id", appended.operation_id.clone()),
                ],
                &request.body,
            )
            .expect("fixed passive output attributes match shared registry");
        let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(&request.agent_id)
            .cloned()
        else {
            self.passive_context_append_result(
                &appended,
                PassiveContextAppendOutcome::Failed {
                    reason: "passive context target is not loaded".to_owned(),
                },
            );
            return;
        };
        self.publish_event_for_agent(
            &cid,
            Some(super::harness_connection_id()),
            Event::AgentPassiveContextAppended(appended),
        );
    }

    /// Reject stale targets and answer previously committed operations before
    /// attempting another semantic append.
    pub(super) fn admit_passive_context_commit(
        &mut self,
        appended: &AgentPassiveContextAppended,
    ) -> bool {
        let previous = self
            .session_runtime
            .agent_store
            .agent(&appended.agent_id)
            .and_then(|tree| tree.passive_context_matches(appended));
        let outcome = if let Some(matches) = previous {
            Some(if matches {
                PassiveContextAppendOutcome::Committed
            } else {
                PassiveContextAppendOutcome::Failed {
                    reason: "passive context operation ID was already used with different content"
                        .to_owned(),
                }
            })
        } else if appended.session_id != self.session_runtime.current_session_id
            || !self
                .agent_runtime
                .agent_registry
                .agent_routes
                .contains_key(&appended.agent_id)
        {
            Some(PassiveContextAppendOutcome::Failed {
                reason: "passive context target is no longer loaded in this session".to_owned(),
            })
        } else {
            None
        };
        if let Some(outcome) = outcome {
            self.passive_context_append_result(appended, outcome);
            false
        } else {
            true
        }
    }

    /// Report the semantic append result after ordinary publication returns.
    /// Queue admission and transient request delivery never count as commit.
    pub(super) fn finish_passive_context_commit(&mut self, appended: &AgentPassiveContextAppended) {
        let committed = self
            .session_runtime
            .agent_store
            .agent(&appended.agent_id)
            .and_then(|tree| tree.passive_context_matches(appended))
            == Some(true);
        self.passive_context_append_result(
            appended,
            if committed {
                PassiveContextAppendOutcome::Committed
            } else {
                PassiveContextAppendOutcome::Failed {
                    reason: "passive context fact failed to commit".to_owned(),
                }
            },
        );
    }

    /// Settle a discarded canonical publication without any external-operation
    /// retry or interpretation of its body.
    pub(super) fn fail_passive_context_append(&mut self, event: &Event, reason: &str) {
        if let Event::AgentPassiveContextAppended(appended) = event {
            self.passive_context_append_result(
                appended,
                PassiveContextAppendOutcome::Failed {
                    reason: reason.chars().take(256).collect(),
                },
            );
        }
    }

    /// Publish a transient, harness-attributed outcome for every observing
    /// client.
    pub(super) fn passive_context_append_result(
        &mut self,
        appended: &AgentPassiveContextAppended,
        outcome: PassiveContextAppendOutcome,
    ) {
        self.publish_event(
            Some(super::harness_connection_id()),
            Event::ExtPassiveContextAppendResult(ExtPassiveContextAppendResult {
                session_id: appended.session_id.clone(),
                agent_id: appended.agent_id.clone(),
                publisher: appended.publisher.clone(),
                operation_id: appended.operation_id.clone(),
                outcome,
            }),
        );
    }
}
