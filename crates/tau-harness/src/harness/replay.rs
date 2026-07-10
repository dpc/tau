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

/// Errors accumulated while replaying catch-up state for one subscriber.
#[derive(Default)]
struct ReplayOutcome {
    /// Session-scoped replay errors, such as membership or restore-log
    /// failures.
    session_errors: Vec<String>,
    /// Per-agent replay errors keyed by the agent whose transcript failed.
    agent_errors: std::collections::BTreeMap<tau_proto::AgentId, Vec<String>>,
}

impl ReplayOutcome {
    fn add_session_error(&mut self, message: String) {
        self.session_errors.push(message);
    }

    fn add_agent_error(&mut self, agent_id: tau_proto::AgentId, message: String) {
        self.agent_errors.entry(agent_id).or_default().push(message);
    }

    fn agent_error(&self, agent_id: &tau_proto::AgentId) -> Option<String> {
        self.agent_errors
            .get(agent_id)
            .map(|errors| errors.join("; "))
    }

    fn session_error(&self) -> Option<String> {
        let mut errors = self.session_errors.clone();
        for (agent_id, agent_errors) in &self.agent_errors {
            for error in agent_errors {
                errors.push(format!("agent `{agent_id}` replay failed: {error}"));
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }
}

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
        historical_selectors: Vec<EventSelector>,
        live_selectors: Vec<EventSelector>,
    ) -> Result<(), RouteError> {
        self.bus
            .set_subscriptions(connection_id, historical_selectors.clone(), live_selectors)?;
        if self.session_initialized(&self.current_session_id) {
            if !historical_selectors.is_empty() {
                self.bus.begin_catch_up(connection_id)?;
            }
            let replay = self.replay_session_events(connection_id, &historical_selectors);
            self.replay_harness_notice(connection_id, &historical_selectors);
            self.emit_session_replay_complete(connection_id, replay.session_error());
            let _ = self.bus.finish_catch_up(connection_id);
        }
        Ok(())
    }

    fn replay_session_events(
        &mut self,
        client_id: &str,
        selectors: &[EventSelector],
    ) -> ReplayOutcome {
        let session_started = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: self.current_session_id.clone(),
            reason: self.current_session_start_reason,
        });
        if selector_matches_event(selectors, &session_started) {
            self.send_catch_up_event(client_id, None, session_started);
        }
        self.replay_session_history(client_id, selectors)
    }

    /// Catches one subscriber up on the bound session's content: the
    /// loaded-agent roster, each agent's durable transcript facts as
    /// replay-marked frames, and currently queued prompts.
    ///
    /// Called from two places: subscribe-time catch-up (after the
    /// `SessionStarted` snapshot above) and session-init completion, where
    /// peers that subscribed before init already saw `SessionStarted` live
    /// and only need the history.
    fn replay_session_history(
        &mut self,
        client_id: &str,
        selectors: &[EventSelector],
    ) -> ReplayOutcome {
        let mut outcome = ReplayOutcome::default();
        let loaded_agents: Vec<tau_proto::AgentId> = {
            match self.store.load_session(self.current_session_id.as_str()) {
                Ok(Some(membership)) => membership.loaded_agents().into_iter().cloned().collect(),
                Ok(None) => Vec::new(),
                Err(error) => {
                    let message = format!("failed to load session events for replay: {error}");
                    self.send_replay_error(client_id, &message);
                    outcome.add_session_error(message);
                    Vec::new()
                }
            }
        };

        match self
            .store
            .session_restore_events(self.current_session_id.as_str())
        {
            Ok(events) => {
                for entry in events {
                    if selector_matches_event(selectors, &entry.event) {
                        let frame =
                            HarnessOutputMessage::deliver_replay(entry.recorded_at, entry.event);
                        let _ = self.bus.send_to(client_id, entry.source.as_deref(), frame);
                    }
                }
            }
            Err(error) => {
                let message = format!("failed to load session restore events for replay: {error}");
                self.send_replay_error(client_id, &message);
                outcome.add_session_error(message);
            }
        }

        for agent_id in &loaded_agents {
            match self.agent_store.load_agent(agent_id.as_str()) {
                Ok(Some(tree)) => {
                    let metadata_events = tree
                        .metadata()
                        .iter()
                        .map(|(key, entry)| {
                            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                                agent_id: agent_id.clone(),
                                key: key.clone(),
                                value: entry.value.clone(),
                                inheritable: entry.inheritable,
                            })
                        })
                        .collect::<Vec<_>>();
                    for event in metadata_events {
                        if selector_matches_event(selectors, &event) {
                            self.send_catch_up_event(client_id, None, event);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let message = format!("failed to load agent `{agent_id}` for replay: {error}");
                    self.send_replay_error(client_id, &message);
                    outcome.add_agent_error(agent_id.clone(), message);
                }
            }
            let event = Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: self.current_session_id.clone(),
                agent_id: agent_id.clone(),
                ephemeral: self.agent_is_ephemeral(agent_id),
            });
            if selector_matches_event(selectors, &event) {
                self.send_catch_up_event(client_id, None, event);
            }
        }

        for agent_id in &loaded_agents {
            let events = match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => events,
                Err(error) => {
                    let message =
                        format!("failed to load agent `{agent_id}` events for replay: {error}");
                    self.send_replay_error(client_id, &message);
                    outcome.add_agent_error(agent_id.clone(), message);
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
        for agent_id in loaded_agents {
            let error = outcome.agent_error(&agent_id);
            self.emit_agent_replay_complete(client_id, agent_id, error);
        }
        outcome
    }

    /// Catches up every already-subscribed peer when session init completes.
    ///
    /// Peers that subscribed before init were skipped by
    /// [`Self::complete_subscription`] — correct for a fresh session, where
    /// everything arrives live. A resumed session's durable history predates
    /// the process and is never published live, so it is replayed here;
    /// otherwise a peer's view would depend on whether it subscribed before
    /// or after init. The `SessionStarted` snapshot is not resent: these
    /// peers just saw it live from `start_session_init`. Current harness
    /// snapshots such as `harness.session_dir` are also replayed here because
    /// configured extensions can subscribe after the live startup notice but
    /// before the session is marked initialized. For fresh sessions the durable
    /// history pass is a no-op (no agents loaded yet), but harness
    /// current-state catch-up is still needed for non-UI peers. UI clients that
    /// are present during startup have already seen the live startup snapshots;
    /// replaying those current-state notices again would duplicate the visible
    /// `harness.session_dir` and `extension.ready` status block.
    pub(crate) fn catch_up_subscribers_after_session_init(&mut self) {
        let subscribers: Vec<(String, tau_proto::ClientKind, Vec<EventSelector>)> = self
            .bus
            .connections()
            .into_iter()
            .filter_map(|meta| {
                let selectors = self.bus.historical_subscriptions(meta.id.as_str())?;
                if selectors.is_empty() {
                    return None;
                }
                Some((meta.id.to_string(), meta.kind, selectors.to_vec()))
            })
            .collect();
        for (client_id, kind, selectors) in subscribers {
            let _ = self.bus.begin_catch_up(&client_id);
            let replay = self.replay_session_history(&client_id, &selectors);
            if kind != tau_proto::ClientKind::Ui {
                self.replay_harness_notice(&client_id, &selectors);
            }
            self.emit_session_replay_complete(&client_id, replay.session_error());
            let _ = self.bus.finish_catch_up(&client_id);
        }
    }

    fn emit_agent_replay_complete(
        &mut self,
        client_id: &str,
        agent_id: tau_proto::AgentId,
        error: Option<String>,
    ) {
        let _ = self.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::AgentReplayComplete(
                tau_proto::AgentReplayComplete {
                    agent_id,
                    session_id: Some(self.current_session_id.clone()),
                    error,
                },
            )),
        );
    }

    fn emit_session_replay_complete(&mut self, client_id: &str, error: Option<String>) {
        let _ = self.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::SessionReplayComplete(
                tau_proto::SessionReplayComplete {
                    session_id: self.current_session_id.clone(),
                    error,
                },
            )),
        );
    }

    /// Replays one newly loaded existing agent to already-live subscribers.
    pub(crate) fn replay_loaded_agent_history_to_subscribers(
        &mut self,
        agent_id: &tau_proto::AgentId,
    ) {
        let subscribers: Vec<(String, Vec<EventSelector>)> = self
            .bus
            .connections()
            .into_iter()
            .filter_map(|meta| {
                let selectors = self.bus.historical_subscriptions(meta.id.as_str())?;
                if selectors.is_empty() {
                    return None;
                }
                Some((meta.id.to_string(), selectors.to_vec()))
            })
            .collect();
        for (client_id, selectors) in subscribers {
            let mut errors = Vec::new();
            match self.agent_store.load_agent(agent_id.as_str()) {
                Ok(Some(tree)) => {
                    let metadata_events = tree
                        .metadata()
                        .iter()
                        .map(|(key, entry)| {
                            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                                agent_id: agent_id.clone(),
                                key: key.clone(),
                                value: entry.value.clone(),
                                inheritable: entry.inheritable,
                            })
                        })
                        .collect::<Vec<_>>();
                    for event in metadata_events {
                        if selector_matches_event(&selectors, &event) {
                            self.send_catch_up_event(&client_id, None, event);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let message = format!("failed to load agent `{agent_id}` for replay: {error}");
                    self.send_replay_error(&client_id, &message);
                    errors.push(message);
                }
            }
            match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => {
                    for entry in events {
                        if selector_matches_event(&selectors, &entry.event)
                            && should_replay_agent_event_to_late_subscriber(&entry.event)
                        {
                            let frame = HarnessOutputMessage::deliver_replay(
                                entry.recorded_at,
                                entry.event,
                            );
                            let _ = self.bus.send_to(&client_id, entry.source.as_deref(), frame);
                        }
                    }
                }
                Err(error) => {
                    let message =
                        format!("failed to load agent `{agent_id}` events for replay: {error}");
                    self.send_replay_error(&client_id, &message);
                    errors.push(message);
                }
            }
            self.emit_agent_replay_complete(
                &client_id,
                agent_id.clone(),
                (!errors.is_empty()).then(|| errors.join("; ")),
            );
        }
    }

    fn send_replay_error(&mut self, client_id: &str, message: &str) {
        self.send_catch_up_event(
            client_id,
            None,
            Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_REPLAY_ERROR.to_owned(),
                message: message.to_owned(),
                level: tau_proto::NoticeLevel::Warning,
                always_show: true,
            }),
        );
    }

    fn send_catch_up_event(&mut self, client_id: &str, source: Option<&str>, event: Event) {
        let _ = self.bus.send_to(
            client_id,
            source,
            HarnessOutputMessage::deliver_replay(tau_proto::UnixMicros::now(), event),
        );
    }

    fn replay_active_queued_prompts(&mut self, client_id: &str, selectors: &[EventSelector]) {
        let mut agent_by_conversation = std::collections::HashMap::new();
        for (agent_id, conversation_id) in &self.agent_routes {
            agent_by_conversation.insert(conversation_id.clone(), agent_id.clone());
        }

        let queued_prompt_events = self
            .agents
            .iter()
            .flat_map(|(conversation_id, conversation)| {
                if conversation.session_id != self.current_session_id {
                    return Vec::new();
                }
                let Some(agent_id) = agent_by_conversation.get(conversation_id).cloned() else {
                    return Vec::new();
                };
                conversation
                    .pending_prompts
                    .iter()
                    .map(|prompt| {
                        Event::AgentPromptQueued(AgentPromptQueued {
                            agent_id: crate::parse_agent_id(&agent_id),
                            text: prompt.text.clone(),
                            message_class: prompt.message_class,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        for event in queued_prompt_events {
            if selector_matches_event(selectors, &event) {
                self.send_catch_up_event(client_id, None, event);
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
            self.send_catch_up_event(client_id, None, session_dir_event);
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
                self.send_catch_up_event(client_id, Some("harness"), event);
            }
        }

        let replayable_harness_notices = self.replayable_harness_notices.clone();
        for info in replayable_harness_notices {
            let event = Event::HarnessNotice(info);
            if selector_matches_event(selectors, &event) {
                self.send_catch_up_event(client_id, Some("harness"), event);
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
                self.send_catch_up_event(client_id, Some("harness"), event);
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
                self.send_catch_up_event(client_id, Some(source_id.as_str()), provider_event);
            }
        }

        for published in self.action_registry.published_schemas() {
            let action_event = Event::ActionSchemaPublished(ActionSchemaPublished {
                extension_name: published.extension_name,
                instance_id: published.instance_id,
                schema: published.schema,
            });
            if selector_matches_event(selectors, &action_event) {
                self.send_catch_up_event(
                    client_id,
                    Some(published.connection_id.as_str()),
                    action_event,
                );
            }
        }

        // Send current model state to the new client.
        let models_event = Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: self.available_models.clone(),
        });
        if selector_matches_event(selectors, &models_event) {
            self.send_catch_up_event(client_id, None, models_event);
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
            self.send_catch_up_event(client_id, None, roles_event);
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
            self.send_catch_up_event(client_id, None, selected_event);
        }
        let context_event = Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: self.current_session_state.context_input_tokens,
            cached_tokens: self.current_session_state.context_cached_tokens,
            percent_used: self.current_session_state.context_percent_used,
        });
        if selector_matches_event(selectors, &context_event) {
            self.send_catch_up_event(client_id, None, context_event);
        }
        let stats_events: Vec<_> = self
            .agents
            .keys()
            .filter_map(|cid| self.agent_stats_snapshot(cid))
            .map(Event::AgentStatsUpdated)
            .collect();
        for event in stats_events {
            if selector_matches_event(selectors, &event) {
                self.send_catch_up_event(client_id, None, event);
            }
        }
        let mut watch_entries = self.agent_watches.iter().collect::<Vec<_>>();
        watch_entries.sort_by_key(|(watcher, _)| *watcher);
        let watch_events: Vec<_> = watch_entries
            .into_iter()
            .filter(|(_, watched)| !watched.is_empty())
            .map(|(watcher, watched)| {
                Event::AgentWatchesUpdated(tau_proto::AgentWatchesUpdated {
                    session_id: self.current_session_id.clone(),
                    watcher_id: crate::parse_agent_id(watcher),
                    watched_agent_ids: watched.iter().map(crate::parse_agent_id).collect(),
                    changed_agent_id: None,
                    cause: tau_proto::AgentWatchUpdateCause::SessionSnapshot,
                })
            })
            .collect();
        for event in watch_events {
            if selector_matches_event(selectors, &event) {
                self.send_catch_up_event(client_id, None, event);
            }
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
            self.send_catch_up_event(client_id, None, effort_levels_event);
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
            self.send_catch_up_event(client_id, None, verbosity_levels_event);
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
            self.send_catch_up_event(client_id, None, thinking_levels_event);
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
            | Event::AgentMessageIncoming(_)
            | Event::AgentMessageOutgoing(_)
            | Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::ToolError(_)
            | Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundError(_)
            | Event::ToolCancelled(_)
            | Event::ProviderResponseFinished(_)
    )
}
