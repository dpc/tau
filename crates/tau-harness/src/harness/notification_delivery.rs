//! Runtime-only trigger readiness for prompt-injected notifications.

use std::collections::HashSet;
use std::time::Instant;

use tau_proto::{AgentId, AgentWorkStatusPhase, ObservationId};

use super::Harness;
use super::subagents_tool::InstalledWaitKind;
use crate::agent::{
    AgentMessageActivationClass, AgentTurnState, DeliveryDeadlineKind, DeliverySchedule,
    PendingMessageWakeSource,
};

/// Deferred side effects for one occurrence that became trigger-ready.
struct ReadyDelivery {
    /// Loaded agent that owns the occurrence.
    agent_id: AgentId,
    /// Durable activation observation used to settle waits.
    activation: Option<ObservationId>,
    /// Message-wake branch node and queued-tool preemption eligibility.
    message_wake: Option<(Option<tau_core::NodeId>, bool)>,
}

impl Harness {
    /// Snapshot the configured delivery policy for one visible user prompt.
    pub(super) fn user_prompt_delivery_schedule(
        &self,
        now: Instant,
    ) -> Result<Option<DeliverySchedule>, String> {
        let policy = self
            .config
            .accepted_harness_settings
            .notification_delivery
            .user_prompt;
        if policy.is_immediate() {
            Ok(None)
        } else {
            DeliverySchedule::new(now, policy).map(Some)
        }
    }

    /// Snapshot the configured delivery policy for one activating message wake.
    pub(super) fn message_wake_delivery_schedule(
        &self,
        source: &PendingMessageWakeSource,
        now: Instant,
    ) -> Result<Option<DeliverySchedule>, String> {
        let policies = self.config.accepted_harness_settings.notification_delivery;
        let policy = match source {
            PendingMessageWakeSource::AgentMessageReceived {
                activation_class: AgentMessageActivationClass::OrdinaryAgentInput,
                ..
            } => policies.agent_message,
            PendingMessageWakeSource::AgentMessageReceived {
                activation_class: AgentMessageActivationClass::IsolatedWatchNotification,
                ..
            } => policies.status,
            PendingMessageWakeSource::MessageFact { .. } => policies.external_message,
        };
        if policy.is_immediate() {
            Ok(None)
        } else {
            DeliverySchedule::new(now, policy).map(Some)
        }
    }

    /// Stamp the latest queued user prompt after admission and installation.
    pub(super) fn arm_latest_user_prompt_delivery(
        &mut self,
        agent_id: &AgentId,
    ) -> (Instant, bool) {
        let admitted_at = Instant::now();
        let schedule = self.user_prompt_delivery_schedule(admitted_at);
        let delayed = matches!(schedule, Ok(Some(_)));
        match schedule {
            Ok(Some(mut schedule)) => {
                if matches!(
                    self.notification_deadline_kind_for(agent_id),
                    DeliveryDeadlineKind::Unavailable
                ) && self.installed_notification_wait_kind(agent_id).is_none()
                {
                    // Active inference has no interruptible cut, but an already
                    // due idle-class prompt becomes sticky-ready for the next
                    // safe continuation. Exact and any waits retain their own
                    // selected deadlines.
                    schedule.mark_ready_at(DeliveryDeadlineKind::Idle, admitted_at);
                }
                if let Some(prompt) = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(agent_id)
                    .and_then(|agent| agent.dispatch.pending_prompts.back_mut())
                {
                    prompt.delivery_schedule = Some(schedule);
                }
            }
            Ok(None) => {}
            Err(error) => self.emit_harness_failure(&error),
        }
        (admitted_at, delayed)
    }

    /// Stamp the latest queued message wake after admission and installation.
    pub(super) fn arm_latest_message_wake_delivery(
        &mut self,
        agent_id: &AgentId,
    ) -> (Instant, bool) {
        let admitted_at = Instant::now();
        let source = self
            .agent_runtime
            .agent_registry
            .agents
            .get(agent_id)
            .and_then(|agent| agent.dispatch.pending_message_wakes.back())
            .map(|wake| wake.source.clone());
        let schedule = source.as_ref().map_or(Ok(None), |source| {
            self.message_wake_delivery_schedule(source, admitted_at)
        });
        let delayed = matches!(schedule, Ok(Some(_)));
        match schedule {
            Ok(Some(schedule)) => {
                if let Some(wake) = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(agent_id)
                    .and_then(|agent| agent.dispatch.pending_message_wakes.back_mut())
                {
                    wake.delivery_schedule = Some(schedule);
                }
            }
            Ok(None) => {}
            Err(error) => self.emit_harness_failure(&error),
        }
        (admitted_at, delayed)
    }

    /// Return the current deadline selector for one loaded agent.
    pub(super) fn notification_deadline_kind_for(
        &self,
        agent_id: &AgentId,
    ) -> DeliveryDeadlineKind {
        match self.installed_notification_wait_kind(agent_id) {
            Some(InstalledWaitKind::ExactTool) => return DeliveryDeadlineKind::WaitTool,
            Some(InstalledWaitKind::Any) => return DeliveryDeadlineKind::WaitAny,
            None => {}
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(agent_id) else {
            return DeliveryDeadlineKind::Unavailable;
        };
        if agent.dispatch.terminating
            || agent.dispatch.in_flight_prompt.is_some()
            || !matches!(
                agent.dispatch.activation_dispatch,
                crate::agent::ActivationDispatchState::None
            )
            || !matches!(agent.turn.turn_state, AgentTurnState::Idle)
            || self.has_deferred_prompt_dispatch_for(agent_id)
            || self.agent_has_open_foreground_tool_round(agent_id)
        {
            return DeliveryDeadlineKind::Unavailable;
        }
        match agent.turn.work_status.phase() {
            AgentWorkStatusPhase::Waiting => DeliveryDeadlineKind::WaitAny,
            AgentWorkStatusPhase::Done | AgentWorkStatusPhase::Blocked => {
                DeliveryDeadlineKind::Idle
            }
            _ if matches!(agent.turn.turn_state, AgentTurnState::Idle) => {
                DeliveryDeadlineKind::Idle
            }
            _ => DeliveryDeadlineKind::Unavailable,
        }
    }

    /// Return the earliest currently applicable notification-delivery deadline.
    pub(super) fn next_notification_delivery_deadline(&self) -> Option<Instant> {
        self.agent_runtime
            .agent_registry
            .agents
            .iter()
            .flat_map(|(agent_id, agent)| {
                let kind = self.notification_deadline_kind_for(agent_id);
                agent
                    .dispatch
                    .pending_prompts
                    .iter()
                    .filter_map(move |prompt| {
                        prompt
                            .delivery_schedule
                            .as_ref()
                            .and_then(|schedule| schedule.deadline(kind))
                    })
                    .chain(
                        agent
                            .dispatch
                            .pending_message_wakes
                            .iter()
                            .filter_map(move |wake| {
                                wake.delivery_schedule
                                    .as_ref()
                                    .and_then(|schedule| schedule.deadline(kind))
                            }),
                    )
            })
            .min()
    }

    /// Mark one bounded due cohort ready before advancing ordinary scheduling.
    pub(super) fn process_notification_delivery_deadlines_at(&mut self, now: Instant) {
        const MAX_READY_PER_CYCLE: usize = 256;

        let agent_ids: Vec<_> = self
            .agent_runtime
            .agent_registry
            .agents
            .keys()
            .cloned()
            .collect();
        let mut activations = Vec::new();
        let mut remaining = MAX_READY_PER_CYCLE;
        for agent_id in agent_ids {
            if remaining == 0 {
                break;
            }
            let kind = self.notification_deadline_kind_for(&agent_id);
            let selected_nodes = self.notification_selected_branch_nodes(&agent_id);
            let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&agent_id) else {
                continue;
            };
            for prompt in &mut agent.dispatch.pending_prompts {
                if remaining == 0 {
                    break;
                }
                if let Some(schedule) = prompt.delivery_schedule.as_mut() {
                    let newly_ready = schedule.mark_ready_at(kind, now);
                    if (newly_ready || schedule.is_ready_at(kind, now))
                        && schedule.take_ready_activation()
                    {
                        remaining -= 1;
                        activations.push(ReadyDelivery {
                            agent_id: agent_id.clone(),
                            activation: prompt.activation_observation,
                            message_wake: None,
                        });
                    }
                }
            }
            for wake in &mut agent.dispatch.pending_message_wakes {
                if remaining == 0 {
                    break;
                }
                let applies = wake
                    .node_id
                    .is_none_or(|node| selected_nodes.contains(&node));
                if let Some(schedule) = wake.delivery_schedule.as_mut() {
                    schedule.mark_ready_at(kind, now);
                    if applies && schedule.take_ready_activation() {
                        remaining -= 1;
                        activations.push(ReadyDelivery {
                            agent_id: agent_id.clone(),
                            activation: wake.activation_observation,
                            message_wake: Some((
                                wake.node_id,
                                matches!(
                                    wake.source,
                                    PendingMessageWakeSource::AgentMessageReceived { .. }
                                ),
                            )),
                        });
                    }
                }
            }
        }
        if activations.is_empty() {
            return;
        }
        for ReadyDelivery {
            agent_id,
            activation,
            message_wake,
        } in activations
        {
            if let Some((node_id, _)) = message_wake
                && !self.notification_wake_applies_to_selected_branch(&agent_id, node_id)
            {
                continue;
            }
            if let Some(activation) = activation {
                self.activate_waits_for(&agent_id, activation);
            }
            if message_wake.is_some_and(|(_, preempts)| preempts) {
                self.preempt_queued_tool_calls_for_message_received(&agent_id);
            }
            self.terminalize_uncertain_marked_owner_for_live_activation(&agent_id);
        }
        self.try_advance_queue();
    }

    /// Process immediate deadlines after one newly admitted runtime obligation.
    pub(super) fn process_new_notification_delivery(&mut self, admitted_at: Instant) {
        self.process_notification_delivery_deadlines_at(admitted_at);
    }

    /// Return whether an unmaterialized or selected-branch wake may act now.
    fn notification_wake_applies_to_selected_branch(
        &self,
        agent_id: &AgentId,
        node_id: Option<tau_core::NodeId>,
    ) -> bool {
        let Some(node_id) = node_id else {
            return true;
        };
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(agent_id) else {
            return false;
        };
        agent
            .identity
            .agent_id
            .as_deref()
            .and_then(|durable_id| self.session_runtime.agent_store.agent(durable_id))
            .is_some_and(|tree| {
                tree.is_ancestor_head(
                    tau_proto::AgentHead::Node(node_id),
                    agent
                        .identity
                        .head
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                )
            })
    }

    /// Materialize selected-branch membership before mutating queued schedules.
    pub(super) fn notification_selected_branch_nodes(
        &self,
        agent_id: &AgentId,
    ) -> HashSet<tau_core::NodeId> {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(agent_id) else {
            return HashSet::new();
        };
        agent
            .identity
            .agent_id
            .as_deref()
            .and_then(|durable_id| self.session_runtime.agent_store.agent(durable_id))
            .map(|tree| {
                tree.branch_node_ids_from(agent.identity.head)
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }
}
