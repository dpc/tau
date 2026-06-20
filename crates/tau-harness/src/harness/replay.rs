//! Late-subscriber replay.
//!
//! Every peer — UI client or extension — that subscribes after the harness
//! has already emitted events is caught up through the same
//! [`Harness::complete_subscription`] path. There is a second catch-up
//! moment: when a session finishes initializing,
//! [`Harness::catch_up_subscribers_after_session_init`] replays the durable
//! session history to every peer that subscribed *before* init — on resume,
//! that history predates the process and is never published live, so without
//! this pass a startup extension would know less than one that joined a
//! second later. Catch-up is semantic state reconstruction, not a readback of
//! a retained event log:
//!
//! - [`Harness::replay_session_events`] announces the current loaded-agent
//!   snapshot, then replays each loaded agent's durable transcript facts from
//!   the global agent store.
//! - [`Harness::replay_harness_notice`] reconstructs current harness status
//!   from live state snapshots, so a subscriber that just joined sees the same
//!   indicators as one that was here from the start without retaining old
//!   runtime events.
//!
//! Historical transcript facts are delivered as replay-marked frames
//! ([`tau_proto::EventDelivery::is_replay`]); side-effecting consumers (sound
//! notifications, tool execution) must skip those frames and react only to
//! live deliveries.

use tau_core::RouteError;
use tau_proto::{
    ActionSchemaPublished, AgentPromptQueued, Event, EventSelector, HarnessContextUsageChanged,
    HarnessModelsAvailable, HarnessOutputMessage, HarnessRoleSelected, HarnessRolesAvailable,
};

use super::agent_runtime_state_for_turn;
use crate::extension::ExtensionState;
use crate::harness::{Harness, selector_matches_event};
use crate::model::{
    baseline_params_for_selection, context_window_for_model, efforts_for_model, role_infos,
    thinking_summaries_for_model, verbosities_for_model,
};

impl Harness {
    /// Completes a `Subscribe` from any peer: installs live routing, then
    /// catches the subscriber up to current state.
    ///
    /// UI clients and extensions share this path on purpose — subscribe
    /// semantics must not drift between peer kinds. Catch-up is skipped while
    /// the current session is still initializing: a subscriber connecting
    /// during startup observes the session lifecycle live, so replaying it
    /// here would deliver duplicate `SessionStarted` announcements. Durable
    /// history a resumed session carries is delivered to those early
    /// subscribers by [`Self::catch_up_subscribers_after_session_init`] once
    /// init completes.
    pub(crate) fn complete_subscription(
        &mut self,
        connection_id: &str,
        selectors: Vec<EventSelector>,
    ) -> Result<(), RouteError> {
        self.bus
            .set_subscriptions(connection_id, selectors.clone())?;
        if self.session_initialized(&self.current_session_id) {
            self.replay_session_events(connection_id, &selectors);
            self.replay_harness_notice(connection_id, &selectors);
        }
        Ok(())
    }

    pub(crate) fn replay_session_events(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let session_started = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: self.current_session_id.clone(),
            reason: self.current_session_start_reason,
        });
        if selector_matches_event(selectors, &session_started) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(session_started),
            );
        }
        self.replay_session_history(client_id, selectors);
    }

    /// Catches one subscriber up on the bound session's content: the
    /// loaded-agent roster, each agent's durable transcript facts as
    /// replay-marked frames, and currently queued prompts.
    ///
    /// Called from two places: subscribe-time catch-up (after the
    /// `SessionStarted` snapshot above) and session-init completion, where
    /// peers that subscribed before init already saw `SessionStarted` live
    /// and only need the history.
    fn replay_session_history(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let loaded_agents: Vec<tau_proto::AgentId> = {
            match self.store.load_session(self.current_session_id.as_str()) {
                Ok(Some(membership)) => membership.loaded_agents().into_iter().cloned().collect(),
                Ok(None) => Vec::new(),
                Err(error) => {
                    self.send_replay_error(
                        client_id,
                        &format!("failed to load session events for replay: {error}"),
                    );
                    Vec::new()
                }
            }
        };

        for agent_id in &loaded_agents {
            if let Ok(Some(tree)) = self.agent_store.load_agent(agent_id.as_str()) {
                for (key, entry) in tree.metadata() {
                    let event = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                        agent_id: agent_id.clone(),
                        key: key.clone(),
                        value: entry.value.clone(),
                        inheritable: entry.inheritable,
                    });
                    if selector_matches_event(selectors, &event) {
                        let _ =
                            self.bus
                                .send_to(client_id, None, HarnessOutputMessage::deliver(event));
                    }
                }
            }
            let event = Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: self.current_session_id.clone(),
                agent_id: agent_id.clone(),
                ephemeral: self.agent_is_ephemeral(agent_id),
            });
            if selector_matches_event(selectors, &event) {
                let _ = self
                    .bus
                    .send_to(client_id, None, HarnessOutputMessage::deliver(event));
            }
        }

        for agent_id in loaded_agents {
            let events = match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => events,
                Err(error) => {
                    self.send_replay_error(
                        client_id,
                        &format!("failed to load agent `{agent_id}` events for replay: {error}"),
                    );
                    continue;
                }
            };
            for entry in events {
                if selector_matches_event(selectors, &entry.event)
                    && should_replay_agent_event_to_late_subscriber(&entry.event)
                {
                    let frame =
                        HarnessOutputMessage::deliver_replay(entry.recorded_at, entry.event);
                    let _ = self.bus.send_to(client_id, entry.source.as_deref(), frame);
                }
            }
        }
        self.replay_active_queued_prompts(client_id, selectors);
    }

    /// Catches up every already-subscribed peer when session init completes.
    ///
    /// Peers that subscribed before init were skipped by
    /// [`Self::complete_subscription`] — correct for a fresh session, where
    /// everything arrives live. A resumed session's durable history predates
    /// the process and is never published live, so it is replayed here;
    /// otherwise a peer's view would depend on whether it subscribed before
    /// or after init. The `SessionStarted` snapshot is not resent: these
    /// peers just saw it live from `start_session_init`. For fresh sessions
    /// this pass is a no-op (no agents loaded yet).
    pub(crate) fn catch_up_subscribers_after_session_init(&mut self) {
        let subscribers: Vec<(String, Vec<EventSelector>)> = self
            .bus
            .connections()
            .into_iter()
            .filter_map(|meta| {
                let selectors = self.bus.subscriptions(meta.id.as_str())?;
                if selectors.is_empty() {
                    return None;
                }
                Some((meta.id.to_string(), selectors.to_vec()))
            })
            .collect();
        for (client_id, selectors) in subscribers {
            self.replay_session_history(&client_id, &selectors);
        }
    }

    fn send_replay_error(&mut self, client_id: &str, message: &str) {
        let _ = self.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_REPLAY_ERROR.to_owned(),
                message: message.to_owned(),
                level: tau_proto::NoticeLevel::Warning,
                always_show: true,
            })),
        );
    }

    fn replay_active_queued_prompts(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let mut agent_by_conversation = std::collections::HashMap::new();
        for (agent_id, conversation_id) in &self.agent_routes {
            agent_by_conversation.insert(conversation_id.clone(), agent_id.clone());
        }

        for (conversation_id, conversation) in &self.agents {
            if conversation.session_id != self.current_session_id {
                continue;
            }
            let target_agent_id = agent_by_conversation.get(conversation_id).cloned();
            for prompt in &conversation.pending_prompts {
                let Some(agent_id) = target_agent_id.clone() else {
                    continue;
                };
                let event = Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: crate::parse_agent_id(&agent_id),
                    text: prompt.text.clone(),
                    message_class: prompt.message_class,
                });
                if selector_matches_event(selectors, &event) {
                    let _ = self
                        .bus
                        .send_to(client_id, None, HarnessOutputMessage::deliver(event));
                }
            }
        }
    }

    /// Replays current harness and extension state to a late-joining client.
    ///
    /// mandatory `harness.notice` diagnostics are replayed here too. In
    /// particular, extension `ConfigError` messages can arrive during daemon
    /// startup before the terminal UI subscribes; replaying them is the
    /// contract that keeps extension config parse failures from becoming
    /// silent fallback behavior.
    ///
    /// Runtime-only historical events are intentionally not replayed here. The
    /// transcript catch-up path above comes from durable agent logs, while this
    /// method reconstructs current harness status snapshots.
    pub(crate) fn replay_harness_notice(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let session_dir_event = self.current_session_dir_event();
        if selector_matches_event(selectors, &session_dir_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(session_dir_event),
            );
        }

        let mut agent_state_events = self
            .agents
            .values()
            .filter(|agent| agent.session_id == self.current_session_id)
            .filter_map(|agent| {
                let agent_id = agent.agent_id.as_ref()?;
                Some(Event::AgentState(tau_proto::AgentStateChanged {
                    agent_id: crate::parse_agent_id(agent_id),
                    state: agent_runtime_state_for_turn(&agent.turn_state),
                }))
            })
            .collect::<Vec<_>>();
        agent_state_events.sort_by(|left, right| match (left, right) {
            (Event::AgentState(left), Event::AgentState(right)) => {
                left.agent_id.as_str().cmp(right.agent_id.as_str())
            }
            _ => std::cmp::Ordering::Equal,
        });
        for event in agent_state_events {
            if selector_matches_event(selectors, &event) {
                let _ = self.bus.send_to(
                    client_id,
                    Some("harness"),
                    HarnessOutputMessage::deliver(event),
                );
            }
        }

        for info in &self.replayable_harness_notices {
            let event = Event::HarnessNotice(info.clone());
            if selector_matches_event(selectors, &event) {
                let _ = self.bus.send_to(
                    client_id,
                    Some("harness"),
                    HarnessOutputMessage::deliver(event),
                );
            }
        }

        let extension_events: Vec<_> = self
            .extensions
            .order
            .iter()
            .filter_map(|connection_id| self.extensions.entries.get(connection_id))
            .map(|entry| match entry.state {
                ExtensionState::Spawning | ExtensionState::Handshaking => {
                    Event::ExtensionStarting(tau_proto::ExtensionStarting {
                        instance_id: entry.instance_id,
                        extension_name: entry.name.clone().into(),
                        pid: entry.pid,
                    })
                }
                ExtensionState::Ready => Event::ExtensionReady(tau_proto::ExtensionReady {
                    instance_id: entry.instance_id,
                    extension_name: entry.name.clone().into(),
                    pid: entry.pid,
                }),
                ExtensionState::Disconnected => {
                    Event::ExtensionExited(tau_proto::ExtensionExited {
                        instance_id: entry.instance_id,
                        extension_name: entry.name.clone().into(),
                        pid: entry.pid,
                        exit_code: None,
                        signal: None,
                    })
                }
            })
            .collect();
        for event in extension_events {
            if selector_matches_event(selectors, &event) {
                let _ = self.bus.send_to(
                    client_id,
                    Some("harness"),
                    HarnessOutputMessage::deliver(event),
                );
            }
        }

        let mut provider_sources: Vec<_> =
            self.provider_models_by_extension.keys().cloned().collect();
        provider_sources.sort();
        for source_id in provider_sources {
            let Some(models) = self.provider_models_by_extension.get(&source_id).cloned() else {
                continue;
            };
            let provider_event =
                Event::ProviderModelsUpdated(tau_proto::ProviderModelsUpdated { models });
            if selector_matches_event(selectors, &provider_event) {
                let _ = self.bus.send_to(
                    client_id,
                    Some(source_id.as_str()),
                    HarnessOutputMessage::deliver(provider_event),
                );
            }
        }

        for published in self.action_registry.published_schemas() {
            let action_event = Event::ActionSchemaPublished(ActionSchemaPublished {
                extension_name: published.extension_name,
                instance_id: published.instance_id,
                schema: published.schema,
            });
            if selector_matches_event(selectors, &action_event) {
                let _ = self.bus.send_to(
                    client_id,
                    Some(published.connection_id.as_str()),
                    HarnessOutputMessage::deliver(action_event),
                );
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            let _ = self
                .bus
                .send_to(client_id, None, HarnessOutputMessage::deliver(models_event));
        }
        let roles_event = Event::HarnessRolesAvailable(HarnessRolesAvailable {
            roles: role_infos(
                &self.provider_model_info,
                &self.available_roles,
                &self.available_models,
            ),
            groups: self.current_role_groups(),
            custom_prompts: self.custom_prompts.clone(),
        });
        if selector_matches_event(selectors, &roles_event) {
            let _ = self
                .bus
                .send_to(client_id, None, HarnessOutputMessage::deliver(roles_event));
        }
        let (harness_settings, _) = crate::settings::load_harness_settings_or_warn(&self.dirs);
        let selected_event = Event::HarnessRoleSelected(HarnessRoleSelected {
            baseline_params: self.selected_model.as_ref().map(|model| {
                baseline_params_for_selection(
                    &harness_settings,
                    &self.provider_model_info,
                    &self.selected_role,
                    model,
                )
            }),
            model_params: self.selected_model_params(),
            model: self.selected_model.clone(),
            context_window: self
                .selected_model
                .as_ref()
                .and_then(|m| context_window_for_model(&self.provider_model_info, m)),
            role: self.selected_role.clone(),
        });
        if selector_matches_event(selectors, &selected_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(selected_event),
            );
        }
        let context_event = Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: self.current_session_state.context_input_tokens,
            cached_tokens: self.current_session_state.context_cached_tokens,
            percent_used: self.current_session_state.context_percent_used,
        });
        if selector_matches_event(selectors, &context_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(context_event),
            );
        }
        let effort_levels = self
            .selected_model
            .as_ref()
            .map(|m| efforts_for_model(&self.provider_model_info, m))
            .unwrap_or_default();
        let effort_levels_event =
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                levels: effort_levels,
            });
        if selector_matches_event(selectors, &effort_levels_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(effort_levels_event),
            );
        }
        let verbosity_levels = self
            .selected_model
            .as_ref()
            .map(|m| verbosities_for_model(&self.provider_model_info, m))
            .unwrap_or_default();
        let verbosity_levels_event =
            Event::HarnessVerbositiesAvailable(tau_proto::HarnessVerbositiesAvailable {
                levels: verbosity_levels,
            });
        if selector_matches_event(selectors, &verbosity_levels_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(verbosity_levels_event),
            );
        }
        let thinking_levels = self
            .selected_model
            .as_ref()
            .map(|m| thinking_summaries_for_model(&self.provider_model_info, m))
            .unwrap_or_default();
        let thinking_levels_event = Event::HarnessThinkingSummariesAvailable(
            tau_proto::HarnessThinkingSummariesAvailable {
                levels: thinking_levels,
            },
        );
        if selector_matches_event(selectors, &thinking_levels_event) {
            let _ = self.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(thinking_levels_event),
            );
        }
    }
}

fn should_replay_agent_event_to_late_subscriber(event: &Event) -> bool {
    // Replay final, durable transcript facts, not progress. In particular, skip
    // provider streaming chunks and prompt-created pending markers, but keep the
    // agent-owned user/assistant/tool facts needed to reconstruct transcript UI.
    matches!(
        event,
        Event::AgentStarted(_)
            | Event::AgentDisplayNameSet(_)
            | Event::AgentPromptSubmitted(_)
            | Event::AgentPromptSteered(_)
            | Event::AgentUserMessageInjected(_)
            | Event::AgentCompactionTriggered(_)
            | Event::AgentMessageSent(_)
            | Event::AgentMessageReceived(_)
            | Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::ToolError(_)
            | Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundError(_)
            | Event::ToolCancelled(_)
            | Event::ProviderResponseFinished(_)
    )
}
