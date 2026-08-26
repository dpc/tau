//! Developer previews and request adapters.
//!
//! Rendered prompts use ordinary ephemeral-agent context initialization.
//! Extension data filesystem operations remain owned by `extension_data`. See
//! [ARCH-tau-harness](../../specs/ARCH-tau-harness.md) for the harness
//! extension-data boundary.

use super::*;

/// A render request awaiting the normal per-agent context initialization path.
pub(super) enum PendingRenderedPrompt {
    /// Render only the system prompt after context initialization.
    System {
        /// Client awaiting the response.
        connection_id: tau_proto::ConnectionId,
        /// Client-selected request correlation.
        request_id: String,
        /// Role to render.
        role: String,
    },
    /// Render the effective prompt after context initialization.
    Prompt {
        /// Client awaiting the response.
        connection_id: tau_proto::ConnectionId,
        /// Client-selected request correlation.
        request_id: String,
        /// Role to render.
        role: String,
        /// Whether to append discovered AGENTS.md context.
        enable_agents_md: bool,
    },
    /// Render the effective model-visible tool surface after context
    /// initialization.
    Tools {
        /// Client awaiting the response.
        connection_id: tau_proto::ConnectionId,
        /// Client-selected request correlation.
        request_id: String,
        /// Role whose effective tool policy is rendered.
        role: String,
    },
}
/// Requests and the readiness deadline owned by one ephemeral preview agent.
pub(super) struct PendingRenderedPreview {
    /// Developer requests waiting on this agent's context.
    pub(super) requests: Vec<PendingRenderedPrompt>,
    /// Absolute deadline for completing the context-ready lifecycle.
    pub(super) deadline: Instant,
}

const RENDERED_PREVIEW_CONTEXT_TIMEOUT: Duration = Duration::from_secs(10);

impl Harness {
    pub(super) fn send_rendered_system_prompt_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::GetRenderedSystemPrompt,
    ) {
        self.queue_rendered_prompt(PendingRenderedPrompt::System {
            connection_id: connection_id.clone(),
            request_id: request.request_id,
            role: request.role,
        });
    }
    pub(super) fn send_rendered_prompt_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::GetRenderedPrompt,
    ) {
        let role = request.role.unwrap_or_else(|| self.selected_role.clone());
        self.queue_rendered_prompt(PendingRenderedPrompt::Prompt {
            connection_id: connection_id.clone(),
            request_id: request.request_id,
            role,
            enable_agents_md: request.enable_agents_md,
        });
    }
    /// Creates an ephemeral preview agent and waits for ordinary context setup.
    fn queue_rendered_prompt(&mut self, request: PendingRenderedPrompt) {
        let role = match &request {
            PendingRenderedPrompt::System { role, .. }
            | PendingRenderedPrompt::Prompt { role, .. }
            | PendingRenderedPrompt::Tools { role, .. } => role.clone(),
        };
        if !self.available_roles.contains_key(&role) {
            self.send_rendered_preview_error(request, format!("unknown role: {role}"));
            return;
        }
        let agent = match self.try_create_user_agent_with_parent(
            self.current_session_id.clone(),
            &role,
            None,
            Vec::new(),
            tau_core::AgentPersistenceMode::Ephemeral,
        ) {
            Ok(agent) => agent,
            Err(error) => {
                self.send_rendered_preview_error(request, error.to_string());
                return;
            }
        };
        let agent_id = self.agent_registry.agents[&agent]
            .agent_id
            .as_deref()
            .map(crate::parse_agent_id)
            .expect("new preview agent has an id");
        self.pending_rendered_prompts.insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![request],
                deadline: Instant::now() + RENDERED_PREVIEW_CONTEXT_TIMEOUT,
            },
        );
        self.complete_rendered_previews(&agent_id);
    }
    /// Responds once a preview agent has the complete runtime template context.
    pub(super) fn complete_rendered_previews(&mut self, agent_id: &tau_proto::AgentId) {
        if !self.frozen_agent_discovery.contains_key(agent_id) {
            return;
        }
        let Some(pending) = self.pending_rendered_prompts.remove(agent_id) else {
            return;
        };
        let cid = self.runtime_agent_id_for_target_agent(Some(agent_id.as_str()));
        for request in pending.requests {
            let role = match &request {
                PendingRenderedPrompt::System { role, .. }
                | PendingRenderedPrompt::Prompt { role, .. }
                | PendingRenderedPrompt::Tools { role, .. } => role.clone(),
            };
            let model = model_for_role(&self.provider_model_info, &self.available_roles, &role);
            let specs = self.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
            if let Some(name) = duplicate_model_visible_tool_name(&specs) {
                self.send_rendered_preview_error(
                    request,
                    format!(
                        "effective tool surface contains duplicate model-visible name `{name}`"
                    ),
                );
                continue;
            }
            match request {
                PendingRenderedPrompt::System {
                    connection_id,
                    request_id,
                    ..
                } => {
                    let result = self
                        .build_system_prompt_for_role_preview_with_snapshot(
                            &role,
                            agent_id,
                            &specs,
                            model.as_ref(),
                        )
                        .map_err(|error| format!("failed to render system prompt: {error}"));
                    self.send_rendered_system_prompt(&connection_id, request_id, result);
                }
                PendingRenderedPrompt::Prompt {
                    connection_id,
                    request_id,
                    enable_agents_md,
                    ..
                } => {
                    let result = self
                        .build_system_prompt_for_role_preview_with_snapshot(
                            &role,
                            agent_id,
                            &specs,
                            model.as_ref(),
                        )
                        .map_err(|error| format!("failed to render system prompt: {error}"));
                    self.send_rendered_prompt(&connection_id, request_id, enable_agents_md, result);
                }
                PendingRenderedPrompt::Tools {
                    connection_id,
                    request_id,
                    ..
                } => {
                    let tools = self.tool_definitions_from_specs(&specs);
                    self.send_rendered_tools(&connection_id, request_id, Ok(tools));
                }
            }
        }
        if let Some(cid) = cid {
            self.remove_agent_expected(&cid);
        }
    }
    /// Sends one failed developer preview response in its variant-specific
    /// shape.
    fn send_rendered_preview_error(&mut self, request: PendingRenderedPrompt, error: String) {
        match request {
            PendingRenderedPrompt::System {
                connection_id,
                request_id,
                ..
            } => {
                self.send_rendered_system_prompt(&connection_id, request_id, Err(error));
            }
            PendingRenderedPrompt::Prompt {
                connection_id,
                request_id,
                enable_agents_md,
                ..
            } => {
                self.send_rendered_prompt(&connection_id, request_id, enable_agents_md, Err(error));
            }
            PendingRenderedPrompt::Tools {
                connection_id,
                request_id,
                ..
            } => {
                self.send_rendered_tools(&connection_id, request_id, Err(error));
            }
        }
    }
    /// Sends a rendered system-prompt completion.
    fn send_rendered_system_prompt(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request_id: String,
        result: Result<String, String>,
    ) {
        let (prompt, error) =
            result.map_or_else(|error| (None, Some(error)), |prompt| (Some(prompt), None));
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedSystemPromptResult(Box::new(
                tau_proto::RenderedSystemPromptResult {
                    request_id,
                    prompt,
                    error,
                },
            )),
        );
    }
    /// Sends a rendered full-prompt completion.
    fn send_rendered_prompt(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request_id: String,
        enable_agents_md: bool,
        result: Result<String, String>,
    ) {
        let result = result.map(|system_prompt| {
            let agents_context = (enable_agents_md && !self.discovered_agents_files.is_empty())
                .then(|| render_agents_context_message(self.discovered_agents_files.iter()));
            render_effective_prompt_message(&system_prompt, agents_context.as_deref())
        });
        let (prompt, error) =
            result.map_or_else(|error| (None, Some(error)), |prompt| (Some(prompt), None));
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedPromptResult(Box::new(tau_proto::RenderedPromptResult {
                request_id,
                prompt,
                error,
            })),
        );
    }
    /// Sends a rendered effective-tool completion.
    fn send_rendered_tools(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request_id: String,
        result: Result<Vec<ToolDefinition>, String>,
    ) {
        let (tools, error) =
            result.map_or_else(|error| (None, Some(error)), |tools| (Some(tools), None));
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedToolDefinitionsResult(Box::new(
                tau_proto::RenderedToolDefinitionsResult {
                    request_id,
                    tools,
                    error,
                },
            )),
        );
    }
    pub(super) fn send_rendered_tool_definitions_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::GetRenderedToolDefinitions,
    ) {
        self.queue_rendered_prompt(PendingRenderedPrompt::Tools {
            connection_id: connection_id.clone(),
            request_id: request.request_id,
            role: request.role.unwrap_or_else(|| self.selected_role.clone()),
        });
    }
    /// Fails expired previews and unloads their context agents.
    pub(super) fn process_rendered_preview_deadlines(&mut self, now: Instant) {
        let expired = self
            .pending_rendered_prompts
            .iter()
            .filter_map(|(agent_id, pending)| (pending.deadline <= now).then_some(agent_id.clone()))
            .collect::<Vec<_>>();
        for agent_id in expired {
            if let Some(pending) = self.pending_rendered_prompts.remove(&agent_id) {
                for request in pending.requests {
                    self.send_rendered_preview_error(
                        request,
                        "timed out waiting for extension agent context readiness".to_owned(),
                    );
                }
            }
            if let Some(cid) = self.runtime_agent_id_for_target_agent(Some(agent_id.as_str())) {
                self.remove_agent_expected(&cid);
            }
        }
    }
    pub(super) fn send_session_agent_list_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::GetSessionAgentList,
    ) {
        let request_id = request.request_id;
        let session_id = request.session_id;
        let result = match self.build_session_agent_list(&session_id, request.scope) {
            Ok(agents) => tau_proto::SessionAgentListResult {
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                result: tau_proto::SessionAgentListResultPayload::Ok { agents },
            },
            Err(error) => tau_proto::SessionAgentListResult {
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                result: tau_proto::SessionAgentListResultPayload::Error { error },
            },
        };
        let mut message = HarnessOutputMessage::SessionAgentListResult(Box::new(result));
        if !session_agent_list_message_fits(&message) {
            message = HarnessOutputMessage::SessionAgentListResult(Box::new(
                tau_proto::SessionAgentListResult {
                    request_id,
                    session_id,
                    result: tau_proto::SessionAgentListResultPayload::Error {
                        error: session_agent_list_error(
                            tau_proto::SessionAgentListErrorKind::ResponseTooLarge,
                            "agent roster response exceeds the protocol message bound",
                        ),
                    },
                },
            ));
        }
        let _ = self.bus.send_to(connection_id, None, message);
    }
    pub(super) fn build_session_agent_list(
        &self,
        session_id: &SessionId,
        scope: tau_proto::SessionAgentListScope,
    ) -> Result<Vec<tau_proto::SessionAgentListEntry>, tau_proto::SessionAgentListError> {
        if session_id != &self.current_session_id {
            return Err(session_agent_list_error(
                tau_proto::SessionAgentListErrorKind::StaleSession,
                "the harness is bound to a different session",
            ));
        }
        if !self.agent_registry.roster_valid
            || !self
                .agent_registry
                .roster_loaded
                .is_subset(&self.agent_registry.roster_ever_loaded)
        {
            return Err(session_agent_list_error(
                tau_proto::SessionAgentListErrorKind::SessionRead,
                "current membership is inconsistent with membership history",
            ));
        }
        let source = match scope {
            tau_proto::SessionAgentListScope::Current => &self.agent_registry.roster_loaded,
            tau_proto::SessionAgentListScope::History => &self.agent_registry.roster_ever_loaded,
        };
        if source.len() > MAX_SESSION_AGENT_LIST_ENTRIES {
            return Err(session_agent_list_error(
                tau_proto::SessionAgentListErrorKind::TooManyAgents,
                "agent roster exceeds the fixed entry bound",
            ));
        }

        let loaded = &self.agent_registry.roster_loaded;
        let mut agent_ids = source.iter().cloned().collect::<Vec<_>>();
        agent_ids.sort();
        let live_agents = self
            .agent_registry
            .agents
            .iter()
            .filter(|(_, agent)| {
                !agent.terminating
                    && agent.session_id == self.current_session_id
                    && agent.agent_id.is_some()
            })
            .filter_map(|(cid, agent)| {
                let agent_id = AgentId::parse(agent.agent_id.as_deref()?).ok()?;
                let navigation_mode = self
                    .agent_registry
                    .navigation_modes
                    .get(&agent_id)
                    .copied()?;
                Some((
                    agent_id,
                    (agent.published_runtime_state, navigation_mode, cid.clone()),
                ))
            })
            .collect::<HashMap<_, _>>();
        let mut remaining_enrichment_bytes = MAX_SESSION_AGENT_LIST_ENRICHMENT_BYTES;
        let mut agents = Vec::with_capacity(agent_ids.len());
        for agent_id in agent_ids {
            let live = loaded
                .contains(&agent_id)
                .then(|| live_agents.get(&agent_id))
                .flatten();
            let lifecycle = match live {
                Some((runtime_state, navigation_mode, _)) => {
                    tau_proto::SessionAgentLifecycle::Live {
                        runtime_state: *runtime_state,
                        navigation_mode: *navigation_mode,
                    }
                }
                None if loaded.contains(&agent_id) => tau_proto::SessionAgentLifecycle::Unavailable,
                None => tau_proto::SessionAgentLifecycle::Unloaded,
            };
            let facts = self
                .agent_store
                .agent_creation_facts(
                    &agent_id,
                    tau_core::AgentCreationFactsBudget {
                        max_record_bytes: MAX_SESSION_AGENT_LIST_FIRST_RECORD_BYTES,
                        remaining_bytes: remaining_enrichment_bytes,
                    },
                )
                .map_err(|_| {
                    session_agent_list_error(
                        tau_proto::SessionAgentListErrorKind::EnrichmentTooLarge,
                        "agent roster exceeds the aggregate enrichment-read bound",
                    )
                })?;
            remaining_enrichment_bytes =
                remaining_enrichment_bytes.saturating_sub(facts.bytes_read());
            let facts = match facts {
                tau_core::AgentCreationFacts::Available {
                    started_at,
                    parent_agent,
                    role,
                    display_name,
                    ..
                } => tau_proto::SessionAgentFacts::Available {
                    started_at,
                    parent_agent,
                    role,
                    display_name,
                },
                tau_core::AgentCreationFacts::Missing => tau_proto::SessionAgentFacts::Missing,
                tau_core::AgentCreationFacts::Invalid { .. } => {
                    tau_proto::SessionAgentFacts::Invalid
                }
                tau_core::AgentCreationFacts::Unreadable { .. } => {
                    tau_proto::SessionAgentFacts::Unreadable
                }
            };
            let work_status = matches!(&lifecycle, tau_proto::SessionAgentLifecycle::Live { .. })
                .then(|| self.agent_registry.agents.get(&agent_id))
                .flatten()
                .map(|agent| {
                    tau_proto::SessionAgentWorkStatus::new(
                        agent.work_status.phase(),
                        agent.work_status.title().map(ToOwned::to_owned),
                    )
                    .expect("harness work status is canonical")
                });
            agents.push(tau_proto::SessionAgentListEntry {
                agent_id: agent_id.clone(),
                lifecycle,
                persistence: if self
                    .agent_store
                    .agent_persistence(agent_id.as_str())
                    .is_ephemeral()
                {
                    tau_proto::SessionAgentPersistence::Ephemeral
                } else {
                    tau_proto::SessionAgentPersistence::Durable
                },
                facts,
                work_status,
                turn_activity: live.map(|(_, _, cid)| self.agent_turn_activity(&agent_id, cid)),
            });
        }
        Ok(agents)
    }
    fn send_extension_data_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request_id: String,
        result: tau_proto::ExtensionDataResultPayload,
    ) {
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::ExtensionDataResult(Box::new(tau_proto::ExtensionDataResult {
                request_id,
                result,
            })),
        );
    }
    pub(super) fn handle_extension_data_request(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::ExtensionDataRequest,
        admission: ExtensionFrameAdmission,
    ) {
        let request_id = request.request_id;
        let secret_scope = request.scope == tau_proto::ExtensionDataScope::Secret;
        let result = match self.run_extension_data_request(
            connection_id,
            request.scope,
            request
                .expected_session_id
                .as_ref()
                .unwrap_or(&admission.session_id),
            request.op,
        ) {
            Ok(value) => tau_proto::ExtensionDataResultPayload::Ok { value },
            Err(error) => tau_proto::ExtensionDataResultPayload::Error {
                kind: error.kind,
                message: if secret_scope {
                    "secret data operation failed".to_owned()
                } else {
                    error.message
                },
            },
        };
        self.send_extension_data_result(connection_id, request_id, result);
    }
    pub(super) fn run_extension_data_request(
        &self,
        connection_id: &tau_proto::ConnectionId,
        scope: tau_proto::ExtensionDataScope,
        expected_session_id: &tau_proto::SessionId,
        op: tau_proto::ExtensionDataRequestOp,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
        if scope == tau_proto::ExtensionDataScope::Session
            && expected_session_id != &self.current_session_id
        {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::SessionMismatch,
                "session target does not match the current session",
            ));
        }
        let is_secret = scope == tau_proto::ExtensionDataScope::Secret;
        let root = self.extension_data_scope_root(connection_id, scope.clone())?;
        match op {
            tau_proto::ExtensionDataRequestOp::ReadFile { path } => {
                if is_secret {
                    run_extension_data_read_file_with_limit(
                        &root,
                        path.into_string(),
                        MAX_SECRET_DATA_FILE_BYTES,
                    )
                } else {
                    run_extension_data_read_file(&root, path.into_string())
                }
            }
            tau_proto::ExtensionDataRequestOp::WriteFile { path, contents } => {
                if is_secret {
                    with_extension_data_scope_lock(&root, || {
                        run_extension_data_write_file_with_limit(
                            &root,
                            path.into_string(),
                            contents,
                            MAX_SECRET_DATA_FILE_BYTES,
                        )
                    })
                } else {
                    run_extension_data_write_file(&root, path.into_string(), contents)
                }
            }
            tau_proto::ExtensionDataRequestOp::CompareAndSwapFile {
                path,
                expected_generation,
                contents,
            } => {
                if is_secret {
                    run_extension_data_compare_and_swap_file(
                        &root,
                        path.into_string(),
                        expected_generation,
                        contents,
                        MAX_SECRET_DATA_FILE_BYTES,
                    )
                } else {
                    Err(ExtensionDataError::new(
                        tau_proto::ExtensionDataErrorKind::Permission,
                        "compare-and-swap is available only for secret data",
                    ))
                }
            }
            tau_proto::ExtensionDataRequestOp::CreateFile { path, contents } => {
                if is_secret {
                    with_extension_data_scope_lock(&root, || {
                        run_extension_data_create_file_with_limit(
                            &root,
                            path.into_string(),
                            contents,
                            MAX_SECRET_DATA_FILE_BYTES,
                        )
                    })
                } else {
                    run_extension_data_create_file(&root, path.into_string(), contents)
                }
            }
            tau_proto::ExtensionDataRequestOp::AppendFile { path, contents } => {
                if is_secret {
                    Err(ExtensionDataError::new(
                        tau_proto::ExtensionDataErrorKind::Permission,
                        "append is unavailable for secret data",
                    ))
                } else {
                    run_scoped_extension_data_append_file(
                        scope,
                        &root,
                        path.into_string(),
                        contents,
                    )
                }
            }
            tau_proto::ExtensionDataRequestOp::DeleteFile { path } => {
                if is_secret {
                    with_extension_data_scope_lock(&root, || {
                        run_extension_data_delete_file(&root, path.into_string())
                    })
                } else {
                    run_extension_data_delete_file(&root, path.into_string())
                }
            }
            tau_proto::ExtensionDataRequestOp::RenameFile { from, to } => {
                if is_secret {
                    with_extension_data_scope_lock(&root, || {
                        run_extension_data_rename_file(&root, from.into_string(), to.into_string())
                    })
                } else {
                    run_extension_data_rename_file(&root, from.into_string(), to.into_string())
                }
            }
            tau_proto::ExtensionDataRequestOp::ListFiles { path } => {
                run_extension_data_list_files(&root, path.into_string())
            }
        }
    }
    fn extension_data_scope_root(
        &self,
        connection_id: &tau_proto::ConnectionId,
        scope: tau_proto::ExtensionDataScope,
    ) -> Result<PathBuf, ExtensionDataError> {
        if self.storage_mode.is_memory_only() {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::Permission,
                "extension data is unavailable in memory-only harnesses",
            ));
        }
        let entry = self.extensions.entries.get(connection_id).ok_or_else(|| {
            ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::Io,
                "unknown extension connection",
            )
        })?;
        if scope == tau_proto::ExtensionDataScope::Secret && entry.supervised_config.is_none() {
            return Err(ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::Permission,
                "secret data is unavailable to in-process extensions",
            ));
        }
        let name = entry.name.as_str();
        tau_config::settings::validate_extension_name(name).map_err(|error| {
            ExtensionDataError::new(
                tau_proto::ExtensionDataErrorKind::InvalidPath,
                error.to_string(),
            )
        })?;
        match scope {
            tau_proto::ExtensionDataScope::Session => {
                if self.storage_mode.is_ephemeral() {
                    return Err(ExtensionDataError::new(
                        tau_proto::ExtensionDataErrorKind::Permission,
                        "session-scoped extension data is unavailable in ephemeral sessions",
                    ));
                }
                Ok(tau_config::settings::sessions_dir_of(&self.state_dir)
                    .join(self.current_session_id.as_str())
                    .join("ext")
                    .join("data")
                    .join(name))
            }
            tau_proto::ExtensionDataScope::User => tau_config::settings::extension_state_dir_of(
                &self.state_dir,
                name,
            )
            .map_err(|error| {
                ExtensionDataError::new(tau_proto::ExtensionDataErrorKind::Io, error.to_string())
            }),
            tau_proto::ExtensionDataScope::Cache => dirs::cache_dir()
                .map(|dir| dir.join("tau").join("ext").join(name))
                .ok_or_else(|| {
                    ExtensionDataError::new(
                        tau_proto::ExtensionDataErrorKind::Io,
                        "could not determine user cache directory",
                    )
                }),
            tau_proto::ExtensionDataScope::Secret => tau_config::settings::extension_secret_dir_of(
                &self.state_dir,
                name,
            )
            .map_err(|error| {
                ExtensionDataError::new(tau_proto::ExtensionDataErrorKind::Io, error.to_string())
            }),
        }
    }
    /// Cancels preview requests and unloads their ephemeral agents before their
    /// requester or session context disappears.
    pub(super) fn cancel_rendered_previews(
        &mut self,
        mut cancel: impl FnMut(&PendingRenderedPrompt) -> bool,
    ) {
        let agent_ids = self
            .pending_rendered_prompts
            .iter()
            .filter_map(|(agent_id, pending)| {
                pending
                    .requests
                    .iter()
                    .any(&mut cancel)
                    .then_some(agent_id.clone())
            })
            .collect::<Vec<_>>();
        for agent_id in agent_ids {
            self.pending_rendered_prompts.remove(&agent_id);
            if let Some(cid) = self.runtime_agent_id_for_target_agent(Some(agent_id.as_str())) {
                self.remove_agent_expected(&cid);
            }
        }
    }
}
