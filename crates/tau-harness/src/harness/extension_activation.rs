//! Owns configured-extension declaration staging and Ready-barrier activation.
//!
//! The activation barrier and interface authority are governed by the
//! persistence and extension-interface gate.

use super::*;

impl Harness {
    pub(super) fn send_agent_prompt_created_result(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        request: tau_proto::GetAgentPromptCreated,
    ) {
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::AgentPromptCreatedResult(Box::new(
                tau_proto::AgentPromptCreatedResult {
                    request_id: request.request_id,
                    prompt: None,
                },
            )),
        );
    }

    pub(super) fn should_stage_extension_capabilities(
        &self,
        source_id: &tau_proto::ConnectionId,
    ) -> bool {
        self.extensions
            .entries
            .get(source_id)
            .is_some_and(|entry| entry.state != ExtensionState::Ready)
    }

    pub(super) fn extension_activation_stage_mut(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) -> &mut ExtensionActivationStage {
        self.extensions
            .activation_staging
            .entry(source_id.clone())
            .or_default()
    }

    pub(super) fn stage_extension_tool_registration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        registration: ToolRegistrationDeclared,
    ) {
        self.extension_activation_stage_mut(source_id)
            .tool_registrations
            .push(registration);
    }

    /// Validate that an extension with an assigned prefix kept every structural
    /// registration identifier inside that prefix envelope.
    pub(super) fn validate_or_reject_assigned_prefix(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        registration: &ToolRegistrationDeclared,
    ) -> bool {
        let Some(entry) = self.extensions.entries.get(source_id) else {
            return true;
        };
        let Some(prefix) = entry.tool_prefix.as_ref() else {
            return true;
        };
        let valid = prefix.contains_tool_name(&registration.tool.name)
            && registration
                .tool
                .model_visible_name
                .as_ref()
                .is_none_or(|name| prefix.contains_tool_name(name))
            && registration
                .tool_group
                .as_ref()
                .is_none_or(|group| prefix.contains_group_name(&group.name));
        if valid {
            return true;
        }
        let extension_name = entry.name.clone();
        let message = format!(
            "Rejected tool registration `{}` from extension `{extension_name}`: assigned tool_prefix `{prefix}` requires internal names, visible aliases, and groups to use the exact `{prefix}_` envelope",
            registration.tool.name
        );
        tracing::warn!(
            target: "tau_harness",
            connection_id = %source_id,
            extension = %extension_name,
            tool_name = %registration.tool.name,
            tool_prefix = %prefix,
            "rejected tool registration outside assigned prefix"
        );
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
            tau_proto::NoticeLevel::Critical,
            tau_proto::NoticePurpose::Alert,
            &message,
        );
        false
    }

    pub(super) fn remove_staged_tool_registration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        tool_name: &ToolName,
    ) -> bool {
        let Some(stage) = self.extensions.activation_staging.get_mut(source_id) else {
            return false;
        };
        let before = stage.tool_registrations.len();
        stage
            .tool_registrations
            .retain(|registration| registration.tool.name != *tool_name);
        stage.tool_registrations.len() != before
    }

    pub(super) fn stage_provider_models_update(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        update: tau_proto::ProviderModelsUpdated,
    ) {
        self.extension_activation_stage_mut(source_id)
            .provider_model_updates
            .push(update);
    }

    pub(super) fn stage_session_discovery_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        snapshot: tau_proto::ExtensionSessionDiscoverySnapshotDeclared,
        admission: ExtensionFrameAdmission,
    ) {
        self.extension_activation_stage_mut(source_id)
            .session_discovery_snapshot = Some(StagedSessionBound {
            admission,
            value: snapshot,
        });
    }

    pub(super) fn stage_agent_discovery_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        snapshot: tau_proto::ExtensionAgentDiscoverySnapshotDeclared,
        admission: ExtensionFrameAdmission,
    ) {
        let key = (
            snapshot.agent_id.clone(),
            snapshot.agent_initialization_id.clone(),
        );
        self.extension_activation_stage_mut(source_id)
            .agent_discovery_snapshots
            .insert(
                key,
                StagedSessionBound {
                    admission,
                    value: snapshot,
                },
            );
    }

    pub(super) fn stage_agent_context_provider_register(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        admission: ExtensionFrameAdmission,
    ) {
        self.extension_activation_stage_mut(source_id)
            .agent_context_provider_admission = Some(admission);
    }

    pub(super) fn stage_session_context_provider_register(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        admission: ExtensionFrameAdmission,
    ) {
        self.extension_activation_stage_mut(source_id)
            .session_context_provider_admission = Some(admission);
    }

    pub(super) fn stage_agent_context_publish(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publish: tau_proto::ExtAgentContextPublish,
        admission: ExtensionFrameAdmission,
    ) {
        self.extension_activation_stage_mut(source_id)
            .agent_context_publishes
            .push(StagedSessionBound {
                admission,
                value: publish,
            });
    }

    pub(super) fn stage_extension_prompt_fragment(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        self.extension_activation_stage_mut(source_id)
            .prompt_fragments
            .insert(publish.fragment.name.clone(), publish.fragment);
    }

    pub(super) fn stage_extension_intercept(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        intercept: tau_proto::Intercept,
    ) {
        self.extension_activation_stage_mut(source_id).intercept = Some(intercept);
    }

    pub(super) fn stage_extension_publish(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        event: Event,
        persist: bool,
    ) {
        self.extension_activation_stage_mut(source_id)
            .emitted_events
            .push(StagedExtensionPublish { event, persist });
    }

    pub(super) fn stage_action_schema(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        schema: tau_actions::ActionSchema,
    ) {
        self.extension_activation_stage_mut(source_id).action_schema = Some(schema);
    }

    pub(super) fn register_extension_tool(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publisher_extension_id: ExtensionName,
        publisher_instance_id: tau_proto::ExtensionInstanceId,
        registration: ToolRegistrationDeclared,
    ) {
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::ToolsChanged);
        let internal_name = registration.tool.name.clone();
        let visible_name = self.tool_model_visible_name(&registration.tool).clone();
        let was_available = !self
            .registry
            .providers_for(internal_name.as_str())
            .is_empty();
        let report = self.registry.register_with_prompt_fragment(
            source_id,
            tau_core::ToolRegistration {
                tool: registration.tool.clone(),
                tool_group: registration.tool_group.clone(),
                prompt_fragment: registration.prompt_fragment.clone(),
            },
        );
        if !report.errors.is_empty() {
            for error in report.errors {
                tracing::warn!(
                    target: "tau_harness",
                    connection_id = %source_id,
                    error = %error,
                    "rejected invalid tool registration"
                );
                self.emit_notice(
                    tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                    tau_proto::NoticeLevel::Critical,
                    tau_proto::NoticePurpose::Alert,
                    &format!("Rejected tool registration from `{source_id}`: {error}"),
                );
            }
            return;
        }
        self.ensure_tool_started_subscription(source_id);
        if !was_available {
            self.mark_tool_available_for_notice(internal_name, visible_name);
        }
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ToolRegister(ToolRegister {
                publisher_extension_id,
                publisher_instance_id,
                tool: registration.tool,
                tool_group: registration.tool_group,
                prompt_fragment: registration.prompt_fragment,
            }),
        );
    }

    pub(super) fn ensure_tool_started_subscription(&mut self, source_id: &tau_proto::ConnectionId) {
        let selector = EventSelector::Exact(tau_proto::EventName::TOOL_STARTED);
        let mut selectors = self
            .bus
            .live_subscriptions(source_id)
            .map_or_else(Vec::new, |s| s.to_vec());
        if selectors.iter().any(|existing| existing == &selector) {
            return;
        }
        selectors.push(selector);
        let historical = self
            .bus
            .historical_subscriptions(source_id)
            .map_or_else(Vec::new, |s| s.to_vec());
        if let Err(error) = self.bus.set_subscriptions(source_id, historical, selectors) {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                %error,
                "could not subscribe tool provider to tool.started"
            );
        }
    }

    pub(super) fn register_extension_interceptor(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        intercept: tau_proto::Intercept,
    ) {
        let component_name = self
            .authenticated_source_name(source_id)
            .expect("authenticated extension source must retain its canonical name");
        self.interceptors.replace_for_connection(
            source_id,
            component_name,
            intercept.selectors,
            intercept.priority,
        );
    }

    pub(super) fn apply_extension_prompt_fragment(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        let contributor = source_id.clone();
        self.extension_prompt_fragments
            .entry(contributor)
            .or_default()
            .insert(publish.fragment.name.clone(), publish.fragment);
    }

    pub(super) fn validate_discovery_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        skills: Vec<tau_proto::DiscoverySkillCandidate>,
        agents_files: Vec<tau_proto::DiscoveryAgentsFile>,
    ) -> Option<ValidatedDiscoverySnapshot> {
        let mut accepted_items = 0usize;
        let mut accepted_bytes = 0usize;
        let mut item_limit_reached = false;
        let mut seen_skills = HashSet::new();
        let mut validated_skills = Vec::new();
        for skill in skills {
            if accepted_items == MAX_DISCOVERY_SNAPSHOT_ITEMS {
                self.emit_info_important(&format!(
                    "discovery snapshot from {source_id} truncated after \
                     {MAX_DISCOVERY_SNAPSHOT_ITEMS} items"
                ));
                item_limit_reached = true;
                break;
            }
            accepted_items += 1;
            let item_bytes = skill
                .name
                .as_str()
                .len()
                .saturating_add(skill.description.len())
                .saturating_add(skill.file_path.as_os_str().len())
                .saturating_add(skill.argument_hint.as_deref().map_or(0, str::len));
            if MAX_DISCOVERY_SNAPSHOT_BYTES < accepted_bytes.saturating_add(item_bytes) {
                self.emit_info_important(&format!(
                    "skill skipped: {} exceeds discovery snapshot bounds",
                    skill.name
                ));
                continue;
            }
            accepted_bytes = accepted_bytes.saturating_add(item_bytes);
            if !seen_skills.insert(skill.name.clone()) {
                self.emit_info_important(&format!(
                    "skill skipped: duplicate `{}` in complete source snapshot",
                    skill.name
                ));
                continue;
            }
            if let Some(message) = tau_skills::skill_name_validation_message(skill.name.as_str()) {
                self.emit_info_important(&format!(
                    "skill skipped: {} from {} has invalid name: {}",
                    skill.name,
                    skill.file_path.display(),
                    message
                ));
                continue;
            }
            if !skill.file_path.is_absolute() {
                self.emit_info_important(&format!(
                    "skill skipped: {} has non-absolute path {}",
                    skill.name,
                    skill.file_path.display()
                ));
                continue;
            }
            let description = tau_skills::truncate_description(&skill.description).into_owned();
            if description != skill.description {
                self.emit_info_important(&format!(
                    "skill skipped: {} description exceeds the supported bound",
                    skill.name
                ));
                continue;
            }
            validated_skills.push((
                skill.name,
                DiscoveredSkill {
                    source_id: source_id.clone(),
                    description,
                    source: DiscoveredSkillSource::File(skill.file_path),
                    add_to_prompt: skill.add_to_prompt,
                    user_invocable: skill.user_invocable || skill.disable_model_invocation,
                    disable_model_invocation: skill.disable_model_invocation,
                    argument_hint: skill.argument_hint,
                    modified: discovery_modified_time(skill.sampled_modified),
                },
            ));
        }

        let mut seen_paths = HashSet::new();
        let mut validated_files = Vec::new();
        for file in agents_files.into_iter().take_while(|_| !item_limit_reached) {
            if accepted_items == MAX_DISCOVERY_SNAPSHOT_ITEMS {
                self.emit_info_important(&format!(
                    "discovery snapshot from {source_id} truncated after \
                     {MAX_DISCOVERY_SNAPSHOT_ITEMS} items"
                ));
                break;
            }
            accepted_items += 1;
            let item_bytes = file
                .file_path
                .as_os_str()
                .len()
                .saturating_add(file.content.len());
            if MAX_DISCOVERY_SNAPSHOT_BYTES < accepted_bytes.saturating_add(item_bytes) {
                self.emit_info_important(&format!(
                    "AGENTS.md skipped: {} exceeds discovery snapshot bounds",
                    file.file_path.display()
                ));
                continue;
            }
            accepted_bytes = accepted_bytes.saturating_add(item_bytes);
            if !file.file_path.is_absolute() {
                self.emit_info_important(&format!(
                    "AGENTS.md skipped: non-absolute path {}",
                    file.file_path.display()
                ));
                continue;
            }
            if MAX_DISCOVERY_AGENTS_FILE_BYTES < file.content.len() {
                self.emit_info_important(&format!(
                    "AGENTS.md skipped: {} exceeds {} bytes",
                    file.file_path.display(),
                    MAX_DISCOVERY_AGENTS_FILE_BYTES
                ));
                continue;
            }
            let path = file.file_path.canonicalize().unwrap_or(file.file_path);
            if !seen_paths.insert(path.clone()) {
                self.emit_info_important(&format!(
                    "AGENTS.md skipped: duplicate canonical path {}",
                    path.display()
                ));
                continue;
            }
            validated_files.push(DiscoveredAgentsFile {
                source_id: source_id.clone(),
                file_path: path,
                content: file.content,
            });
        }
        Some((validated_skills, validated_files))
    }

    pub(super) fn apply_session_discovery_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        snapshot: tau_proto::ExtensionSessionDiscoverySnapshotDeclared,
    ) {
        if snapshot.session_id != self.current_session_id {
            return;
        }
        let Some((skills, agents_files)) =
            self.validate_discovery_snapshot(source_id, snapshot.skills, snapshot.agents_files)
        else {
            return;
        };
        replace_discovery_source(
            &mut self.discovered_skill_candidates,
            &mut self.discovered_skills,
            &mut self.discovered_agents_files,
            source_id,
            skills,
            agents_files,
        );
        self.publish_session_skills_projection();
        self.record_session_init_provider_progress(source_id);
    }

    pub(super) fn apply_agent_discovery_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        snapshot: tau_proto::ExtensionAgentDiscoverySnapshotDeclared,
    ) {
        if snapshot.session_id != self.current_session_id {
            return;
        }
        let Some(pending) = self.pending_agent_discovery.get(&snapshot.agent_id) else {
            return;
        };
        if pending.initialization_id != snapshot.agent_initialization_id {
            return;
        }
        let Some((skills, agents_files)) =
            self.validate_discovery_snapshot(source_id, snapshot.skills, snapshot.agents_files)
        else {
            return;
        };
        let Some(pending) = self.pending_agent_discovery.get_mut(&snapshot.agent_id) else {
            return;
        };
        replace_discovery_source(
            &mut pending.skill_candidates,
            &mut pending.skills,
            &mut pending.agents_files,
            source_id,
            skills,
            agents_files,
        );
    }

    pub(super) fn publish_session_skills_projection(&mut self) {
        let snapshot = tau_proto::HarnessSessionSkillsAvailable {
            session_id: self.current_session_id.clone(),
            skills: effective_skills(&self.discovered_skills),
        };
        self.session_skills_available = snapshot.clone();
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessSessionSkillsAvailable(snapshot),
        );
    }

    /// Reconciles restored checkpoints against discovered models.
    ///
    /// [`RestoredCheckpointAuthority::DiscoveryComplete`] makes every missing
    /// route authoritative. [`RestoredCheckpointAuthority::ExplicitlyRemoved`]
    /// authorizes only models removed from one ready provider's later snapshot;
    /// other missing models continue waiting.
    pub(super) fn resume_restored_compaction_checkpoints(
        &mut self,
        authority: RestoredCheckpointAuthority<'_>,
    ) {
        let all_absence_is_authoritative =
            matches!(authority, RestoredCheckpointAuthority::DiscoveryComplete);
        let authoritatively_removed_models = match authority {
            RestoredCheckpointAuthority::DiscoveryComplete => None,
            RestoredCheckpointAuthority::ExplicitlyRemoved(models) => Some(models),
        };
        if self.provider_model_info.is_empty()
            && !all_absence_is_authoritative
            && authoritatively_removed_models.is_none_or(HashSet::is_empty)
        {
            return;
        }
        let pending =
            self.agents
                .iter()
                .filter_map(|(cid, agent)| match &agent.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                        owner:
                            path_crate_agent::InferenceCheckpointOwner::Standalone {
                                id: transaction_id,
                            },
                        agent_prompt_id,
                        through,
                        dispatch,
                    } => Some(RestoredCompactionCheckpoint {
                        cid: cid.clone(),
                        agent_id: crate::parse_agent_id(agent.agent_id.as_deref()?),
                        transaction_id: transaction_id.clone(),
                        agent_prompt_id: agent_prompt_id.clone(),
                        through: *through,
                        dispatch: dispatch.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
        for checkpoint in pending {
            let model = &checkpoint.dispatch.model;
            let absence_is_authoritative = all_absence_is_authoritative
                || authoritatively_removed_models.is_some_and(|models| models.contains(model));
            if !self.provider_model_info.contains_key(model) && !absence_is_authoritative {
                continue;
            }
            let key = (
                checkpoint.agent_id.clone(),
                checkpoint.transaction_id.clone(),
            );
            let event =
                Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                    agent_id: checkpoint.agent_id,
                    transaction_id: Some(checkpoint.transaction_id),
                    agent_prompt_id: checkpoint.agent_prompt_id,
                    through: checkpoint.through,
                    model: Some(checkpoint.dispatch.model),
                    operation: Some(checkpoint.dispatch.operation),
                    activation_cut: Some(checkpoint.dispatch.activation_cut),
                    output_length_continuation: None,
                });
            if !self.activation_successor_matches_selected_head(&event)
                || !self.enqueued_standalone_inference_checkpoints.insert(key)
            {
                continue;
            }
            self.publish_for_agent(&checkpoint.cid, event);
        }
    }

    pub(super) fn extension_action_owner(
        &self,
        source_id: &tau_proto::ConnectionId,
    ) -> (ExtensionName, tau_proto::ExtensionInstanceId) {
        if let Some(extension) = self.extensions.entries.get(source_id) {
            return (extension.name.clone(), extension.instance_id);
        }
        (
            self.authenticated_source_name(source_id)
                .expect("authenticated extension source must retain its canonical name"),
            0.into(),
        )
    }

    pub(super) fn publish_action_schema(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        extension_name: ExtensionName,
        instance_id: tau_proto::ExtensionInstanceId,
        schema: tau_actions::ActionSchema,
    ) {
        if self
            .action_registry
            .schema_for_connection(source_id)
            .is_some_and(|current| {
                current.extension_name == extension_name
                    && current.instance_id == instance_id
                    && current.schema == schema
            })
        {
            return;
        }
        if let Err(error) = self.action_registry.register_schema(
            source_id,
            extension_name.clone(),
            instance_id,
            schema.clone(),
        ) {
            self.emit_harness_failure(&format!(
                "extension {extension_name} published invalid action schema: {error}"
            ));
            return;
        }
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ActionSchemaPublished(ActionSchemaPublished {
                extension_name,
                instance_id,
                schema,
            }),
        );
    }

    pub(super) fn apply_agent_context_provider_registration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.agent_context_providers.insert(source_id.clone());
    }

    pub(super) fn apply_session_context_provider_registration(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        self.session_context_providers.insert(source_id.clone());
    }

    pub(super) fn apply_agent_context_publish(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publish: tau_proto::ExtAgentContextPublish,
    ) {
        let tau_proto::ExtAgentContextPublish {
            session_id,
            agent_initialization_id,
            agent_id,
            key,
            value,
        } = publish;
        let matches_pending = self
            .pending_agent_discovery
            .get(&agent_id)
            .is_some_and(|pending| pending.initialization_id == agent_initialization_id);
        let matches_frozen = self
            .frozen_agent_discovery
            .get(&agent_id)
            .is_some_and(|frozen| frozen.initialization_id == agent_initialization_id);
        if session_id != self.current_session_id || !(matches_pending || matches_frozen) {
            return;
        }
        let contributor = source_id.clone();
        let extension_name = self
            .authenticated_source_name(&contributor)
            .expect("authenticated extension source must retain its canonical name");
        self.agent_context.publish(
            agent_id,
            key,
            contributor,
            extension_name.to_string(),
            value,
        );
    }

    pub(super) fn apply_extension_context_ready(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        self.handle_extension_context_ready(source_id, ready)
    }

    pub(super) fn apply_extension_session_context_ready(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        ready: tau_proto::ExtensionSessionContextReady,
    ) -> Result<(), HarnessError> {
        self.handle_extension_session_context_ready(source_id, ready)
    }

    pub(super) fn extension_frame_admission_is_current(
        &self,
        admission: &ExtensionFrameAdmission,
    ) -> bool {
        admission.session_id == self.current_session_id
            && admission.session_generation == self.current_session_generation
    }

    pub(super) fn activate_staged_extension_capabilities(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) -> Vec<DeferredExtensionMessage> {
        let Some(stage) = self.extensions.activation_staging.remove(source_id) else {
            return Vec::new();
        };
        if let Some(intercept) = stage.intercept {
            self.register_extension_interceptor(source_id, intercept);
        }
        let tool_publisher = self
            .extensions
            .entries
            .get(source_id)
            .map(|entry| (entry.name.clone(), entry.instance_id));
        for registration in stage.tool_registrations {
            if let Some((publisher_extension_id, publisher_instance_id)) = tool_publisher.clone() {
                self.register_extension_tool(
                    source_id,
                    publisher_extension_id,
                    publisher_instance_id,
                    registration,
                );
            }
        }
        let mut staged_model_ids = HashSet::new();
        let mut final_provider_models = None;
        for update in stage.provider_model_updates {
            staged_model_ids.extend(update.models.iter().map(|model| model.id.clone()));
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderModelsUpdated(update.clone()),
            );
            final_provider_models = Some(update.models);
        }
        if let Some(models) = final_provider_models {
            let final_model_ids = models
                .iter()
                .map(|model| model.id.clone())
                .collect::<HashSet<_>>();
            let removed_before_ready = staged_model_ids
                .difference(&final_model_ids)
                .cloned()
                .collect::<HashSet<_>>();
            self.apply_provider_models_snapshot(source_id, models);
            self.resume_restored_compaction_checkpoints(
                RestoredCheckpointAuthority::ExplicitlyRemoved(&removed_before_ready),
            );
        }
        if let Some(schema) = stage.action_schema {
            let (extension_name, instance_id) = self.extension_action_owner(source_id);
            self.publish_action_schema(source_id, extension_name, instance_id, schema);
        }
        if let Some(staged) = stage.session_discovery_snapshot
            && self.extension_frame_admission_is_current(&staged.admission)
        {
            self.apply_session_discovery_snapshot(source_id, staged.value);
        }
        for staged in stage.agent_discovery_snapshots.into_values() {
            if self.extension_frame_admission_is_current(&staged.admission) {
                self.apply_agent_discovery_snapshot(source_id, staged.value);
            }
        }
        if stage
            .agent_context_provider_admission
            .as_ref()
            .is_some_and(|admission| self.extension_frame_admission_is_current(admission))
        {
            self.apply_agent_context_provider_registration(source_id);
        }
        if stage
            .session_context_provider_admission
            .as_ref()
            .is_some_and(|admission| self.extension_frame_admission_is_current(admission))
        {
            self.apply_session_context_provider_registration(source_id);
        }
        for staged in stage.agent_context_publishes {
            if self.extension_frame_admission_is_current(&staged.admission) {
                self.apply_agent_context_publish(source_id, staged.value);
            }
        }
        for fragment in stage.prompt_fragments.into_values() {
            self.apply_extension_prompt_fragment(
                source_id,
                tau_proto::ExtPromptFragmentPublish { fragment },
            );
        }
        for staged in stage.emitted_events {
            self.enqueue_publish(Some(source_id), staged.event, staged.persist, false, None);
        }
        stage.deferred_messages
    }

    /// Release one ready extension's deferred operational messages.
    pub(super) fn finish_staged_extension_activation(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        deferred_messages: Vec<DeferredExtensionMessage>,
    ) -> Result<(), HarnessError> {
        for deferred in deferred_messages {
            self.handle_extension_message_with_admission(
                source_id,
                deferred.message,
                deferred.admission,
            )?;
        }
        Ok(())
    }

    /// Return whether every initially configured extension has reached a
    /// terminal startup state and the global registration preflight may
    /// run.
    pub(super) fn initial_extension_preflight_ready(&self) -> bool {
        self.extensions.pending_connects == 0
            && self
                .extensions
                .pending_provider_model_declarations
                .is_empty()
            && self
                .extensions
                .pending_tool_lifecycle_declarations
                .is_empty()
            && self
                .extensions
                .pending_action_schema_declarations
                .is_empty()
            && self
                .extensions
                .pending_prompt_fragment_declarations
                .is_empty()
            && self
                .extensions
                .pending_session_discovery_declarations
                .is_empty()
            && self
                .extensions
                .pending_agent_context_declarations
                .is_empty()
            && self
                .extensions
                .entries
                .iter()
                .all(|(connection_id, entry)| {
                    matches!(
                        entry.state,
                        ExtensionState::Ready | ExtensionState::Disconnected
                    ) || self.extensions.ready_received.contains(connection_id)
                })
    }

    /// Resolve all staged initial tool-name collisions independently of Ready
    /// arrival order.
    pub(super) fn preflight_initial_extension_tools(&mut self) -> Result<(), HarnessError> {
        #[derive(Clone)]
        struct Owner {
            connection_id: tau_proto::ConnectionId,
            instance_name: String,
            required: bool,
            prefix: Option<tau_proto::ToolNamePrefix>,
        }

        for stage in self.extensions.activation_staging.values_mut() {
            let mut seen = HashSet::new();
            let mut registrations = stage
                .tool_registrations
                .drain(..)
                .rev()
                .filter(|registration| seen.insert(registration.tool.name.clone()))
                .collect::<Vec<_>>();
            registrations.reverse();
            stage.tool_registrations = registrations;
        }

        let mut invalid = Vec::new();
        for (connection_id, stage) in &self.extensions.activation_staging {
            let Some(entry) = self.extensions.entries.get(connection_id) else {
                continue;
            };
            for registration in &stage.tool_registrations {
                if let Err(error) = tau_core::validate_tool_examples(&registration.tool) {
                    invalid.push((
                        connection_id.clone(),
                        entry.name.clone(),
                        entry.require,
                        error.to_string(),
                    ));
                    break;
                }
            }
        }
        invalid.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        for (connection_id, name, required, error) in invalid {
            self.emit_notice(
                tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                tau_proto::NoticeLevel::Critical,
                tau_proto::NoticePurpose::Alert,
                &format!("Rejected tool registration from `{name}`: {error}"),
            );
            if required {
                return Err(HarnessError::Participant(format!(
                    "required extension `{name}` published an invalid tool registration: {error}"
                )));
            }
            self.disable_optional_extension(
                &connection_id,
                &format!(
                    "optional extension `{name}` disabled after invalid tool registration: {error}"
                ),
            );
        }

        let mut owners_by_tool = HashMap::<ToolName, Vec<Owner>>::new();
        for (connection_id, stage) in &self.extensions.activation_staging {
            let Some(entry) = self.extensions.entries.get(connection_id) else {
                continue;
            };
            for registration in &stage.tool_registrations {
                let owners = owners_by_tool
                    .entry(registration.tool.name.clone())
                    .or_default();
                if owners
                    .iter()
                    .any(|owner| owner.connection_id == *connection_id)
                {
                    continue;
                }
                owners.push(Owner {
                    connection_id: connection_id.clone(),
                    instance_name: entry.name.to_string(),
                    required: entry.require,
                    prefix: entry.tool_prefix.clone(),
                });
            }
        }

        let mut owners_by_tool = owners_by_tool.into_iter().collect::<Vec<_>>();
        owners_by_tool.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
        let mut disabled_optional = BTreeMap::<tau_proto::ConnectionId, String>::new();
        for (tool_name, mut owners) in owners_by_tool {
            owners.sort_by(|a, b| {
                a.instance_name
                    .cmp(&b.instance_name)
                    .then_with(|| a.connection_id.cmp(&b.connection_id))
            });
            let internal_owner = self
                .registry
                .providers_for(tool_name.as_str())
                .into_iter()
                .any(|provider| provider.kind == tau_core::ToolProviderKind::Internal);
            let required = owners
                .iter()
                .filter(|owner| owner.required)
                .collect::<Vec<_>>();
            let conflict = internal_owner || owners.len() > 1;
            if !conflict {
                continue;
            }
            let owner_label = |owner: &Owner| {
                format!(
                    "{}{}",
                    owner.instance_name,
                    owner
                        .prefix
                        .as_ref()
                        .map_or_else(String::new, |prefix| format!(" (prefix `{prefix}`)"))
                )
            };
            if internal_owner {
                if let Some(owner) = required.first() {
                    return Err(HarnessError::Participant(format!(
                        "required extension `{}` conflicts with harness-internal tool `{tool_name}`",
                        owner_label(owner)
                    )));
                }
                for owner in owners {
                    disabled_optional
                        .entry(owner.connection_id.clone())
                        .or_insert_with(|| {
                            format!(
                                "optional extension `{}` disabled because final tool name `{tool_name}` conflicts with a harness-internal tool",
                                owner_label(&owner)
                            )
                        });
                }
                continue;
            }
            if required.len() > 1 {
                return Err(HarnessError::Participant(format!(
                    "required extensions {} register the same final tool name `{tool_name}`",
                    required
                        .iter()
                        .map(|owner| owner_label(owner))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if required.len() == 1 {
                let winner = owner_label(required[0]);
                for owner in owners.iter().filter(|owner| !owner.required) {
                    disabled_optional
                        .entry(owner.connection_id.clone())
                        .or_insert_with(|| {
                            format!(
                                "optional extension `{}` disabled because final tool name `{tool_name}` conflicts with required extension `{winner}`",
                                owner_label(owner)
                            )
                        });
                }
            } else {
                let claimants = owners
                    .iter()
                    .map(&owner_label)
                    .collect::<Vec<_>>()
                    .join(", ");
                for owner in &owners {
                    disabled_optional
                        .entry(owner.connection_id.clone())
                        .or_insert_with(|| {
                            format!(
                                "optional extension `{}` disabled because final tool name `{tool_name}` is also claimed by optional extensions {claimants}",
                                owner_label(owner)
                            )
                        });
                }
            }
        }

        for (connection_id, message) in disabled_optional {
            self.disable_optional_extension(&connection_id, &message);
        }
        Ok(())
    }

    /// Activate one extension that has sent Ready, then publish its lifecycle
    /// readiness only after its complete staged batch succeeds.
    pub(super) fn finish_ready_extension_activation(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) -> Result<(), HarnessError> {
        if !self.extensions.ready_received.contains(source_id) {
            return Ok(());
        }
        if self
            .extensions
            .pending_provider_model_declarations
            .contains_key(source_id)
            || self
                .extensions
                .pending_tool_lifecycle_declarations
                .contains_key(source_id)
            || self
                .extensions
                .pending_action_schema_declarations
                .contains_key(source_id)
            || self
                .extensions
                .pending_prompt_fragment_declarations
                .contains_key(source_id)
            || self
                .extensions
                .pending_session_discovery_declarations
                .contains_key(source_id)
            || self
                .extensions
                .pending_agent_context_declarations
                .contains_key(source_id)
        {
            return Ok(());
        }
        let activated = self.activate_staged_extension_capabilities(source_id);
        self.extensions.ready_received.remove(source_id);
        self.set_extension_state(source_id, ExtensionState::Ready);
        self.emit_extension_ready(source_id);
        self.finish_staged_extension_activation(source_id, activated)
    }

    /// Complete the global initial stage barrier, or activate one post-startup
    /// respawn stage without winner selection.
    pub(super) fn maybe_finish_extension_activation(
        &mut self,
        source_id: Option<&tau_proto::ConnectionId>,
    ) -> Result<(), HarnessError> {
        if self.initial_extension_tool_preflight_complete {
            return source_id.map_or(Ok(()), |source_id| {
                self.finish_ready_extension_activation(source_id)
            });
        }
        if !self.initial_extension_preflight_ready() {
            return Ok(());
        }
        self.resolving_initial_extension_collisions = true;
        let preflight = self.preflight_initial_extension_tools();
        if let Err(error) = preflight {
            self.resolving_initial_extension_collisions = false;
            return Err(error);
        }
        self.initial_extension_tool_preflight_complete = true;
        let mut ready = self
            .extensions
            .ready_received
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        ready.sort();
        let mut deferred_by_connection = Vec::with_capacity(ready.len());
        for connection_id in &ready {
            deferred_by_connection.push((
                connection_id.clone(),
                self.activate_staged_extension_capabilities(connection_id),
            ));
        }
        for connection_id in &ready {
            self.extensions.ready_received.remove(connection_id);
            self.set_extension_state(connection_id, ExtensionState::Ready);
        }
        for connection_id in &ready {
            self.emit_extension_ready(connection_id);
        }
        let mut deferred_messages = Vec::new();
        for (connection_id, messages) in deferred_by_connection {
            deferred_messages.extend(
                messages
                    .into_iter()
                    .map(|deferred| (deferred.order, connection_id.clone(), deferred)),
            );
        }
        deferred_messages.sort_by_key(|(order, _, _)| *order);
        for (_, connection_id, deferred) in deferred_messages {
            self.handle_extension_message_with_admission(
                &connection_id,
                deferred.message,
                deferred.admission,
            )?;
        }
        self.resolving_initial_extension_collisions = false;
        self.drain_pending_tool_invocations()?;
        self.try_advance_queue();
        Ok(())
    }

    /// Apply initial required/optional policy to a malformed handshake. After
    /// the initial barrier, isolate only the failed connection and retain
    /// normal respawn policy.
    pub(super) fn handle_extension_protocol_failure(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: String,
    ) -> Result<(), HarnessError> {
        let lifecycle = self
            .extensions
            .entries
            .get(source_id)
            .map(|entry| (entry.name.clone(), entry.require));
        let Some((name, required)) = lifecycle else {
            return Err(HarnessError::Participant(message));
        };
        let initial_startup = !self.initial_extension_tool_preflight_complete;
        if required && initial_startup {
            return Err(HarnessError::Participant(message));
        }
        tracing::warn!(
            target: "tau_harness::startup",
            connection_id = %source_id,
            error = %message,
            "isolating extension after protocol failure"
        );
        if initial_startup {
            self.disable_optional_extension(
                source_id,
                &format!("optional extension `{name}` disabled after protocol failure: {message}"),
            );
        } else {
            self.emit_notice(
                tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                tau_proto::NoticeLevel::Critical,
                tau_proto::NoticePurpose::Alert,
                &format!("extension `{name}` disconnected after protocol failure: {message}"),
            );
            self.handle_disconnect(source_id);
        }
        self.maybe_finish_extension_activation(Some(source_id))
    }

    /// Charge one retained pre-activation frame to bounded per-connection
    /// quotas.
    pub(super) fn reserve_extension_activation_message(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: &HarnessInputMessage,
    ) -> Result<bool, HarnessError> {
        struct BoundedCounter {
            len: usize,
            limit: usize,
            exceeded: bool,
        }
        impl std::io::Write for BoundedCounter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let Some(next) = self.len.checked_add(bytes.len()) else {
                    self.exceeded = true;
                    return Err(path_std_io::Error::other("activation frame exceeds quota"));
                };
                if self.limit < next {
                    self.exceeded = true;
                    return Err(path_std_io::Error::other("activation frame exceeds quota"));
                }
                self.len = next;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (current_count, current_bytes) = {
            let stage = self.extension_activation_stage_mut(source_id);
            (stage.retained_message_count, stage.retained_message_bytes)
        };
        let mut counter = BoundedCounter {
            len: 0,
            limit: MAX_EXTENSION_ACTIVATION_BYTES.saturating_sub(current_bytes),
            exceeded: false,
        };
        let encoded = ciborium::into_writer(message, &mut counter);
        let next_count = current_count.saturating_add(1);
        if counter.exceeded || MAX_EXTENSION_ACTIVATION_MESSAGES < next_count {
            let message = format!(
                "extension activation staging exceeds {} messages or {} encoded bytes",
                MAX_EXTENSION_ACTIVATION_MESSAGES, MAX_EXTENSION_ACTIVATION_BYTES
            );
            self.handle_extension_protocol_failure(source_id, message)?;
            return Ok(false);
        }
        encoded.map_err(|error| {
            HarnessError::Participant(format!(
                "failed to size extension activation frame: {error}"
            ))
        })?;
        let next_bytes = current_bytes + counter.len;
        let stage = self.extension_activation_stage_mut(source_id);
        stage.retained_message_count = next_count;
        stage.retained_message_bytes = next_bytes;
        Ok(true)
    }

    /// Retain one operational frame behind activation with global wire order.
    pub(super) fn defer_extension_activation_message(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: HarnessInputMessage,
        admission: ExtensionFrameAdmission,
    ) {
        let order = self.next_deferred_extension_message_order;
        self.next_deferred_extension_message_order =
            self.next_deferred_extension_message_order.saturating_add(1);
        self.extension_activation_stage_mut(source_id)
            .deferred_messages
            .push(DeferredExtensionMessage {
                order,
                admission,
                message,
            });
    }

    #[cfg(test)]
    pub(super) fn handle_extension_message(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: impl Into<HarnessInputMessage>,
    ) -> Result<(), HarnessError> {
        let message = message.into();
        let frame_bytes = tau_proto::ProtocolMessageBytes::new(
            tau_proto::encode_message_to_vec(&message)
                .expect("synthetic extension message must encode")
                .len() as u64,
        )
        .expect("an encoded extension message is nonempty");
        self.handle_extension_message_with_frame_bytes(source_id, message, frame_bytes)
    }

    pub(super) fn handle_extension_message_with_frame_bytes(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: impl Into<HarnessInputMessage>,
        frame_bytes: tau_proto::ProtocolMessageBytes,
    ) -> Result<(), HarnessError> {
        let message = message.into();
        if let Some(entry) = self.extensions.entries.get(source_id) {
            entry
                .protocol_io
                .record_uplink_frame_bytes(&message, frame_bytes);
        }
        let admission = self.current_extension_frame_admission();
        self.handle_extension_message_with_admission(source_id, message, admission)
    }

    pub(super) fn current_extension_frame_admission(&self) -> ExtensionFrameAdmission {
        ExtensionFrameAdmission {
            session_id: self.current_session_id.clone(),
            session_generation: self.current_session_generation,
        }
    }

    pub(super) fn handle_extension_message_with_admission(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        message: HarnessInputMessage,
        admission: ExtensionFrameAdmission,
    ) -> Result<(), HarnessError> {
        self.retry_pending_agent_publications();
        if let Some(entry) = self.extensions.entries.get(source_id) {
            if entry.state == ExtensionState::Disconnected
                && matches!(message, HarnessInputMessage::ExtensionNoticeRequest(_))
            {
                // A stale/disconnected origin has no notice authority and no
                // remaining protocol connection to isolate. Preserve metering,
                // then deny without manufacturing a protocol diagnostic.
                return Ok(());
            }
            let ready_received = self.extensions.ready_received.contains(source_id);
            let legal = if ready_received {
                matches!(
                    message,
                    HarnessInputMessage::Disconnect(_)
                        | HarnessInputMessage::Subscribe(_)
                        | HarnessInputMessage::Intercept(_)
                        | HarnessInputMessage::Emit(_)
                        | HarnessInputMessage::ConfigError(_)
                        | HarnessInputMessage::ExtensionNoticeRequest(_)
                        | HarnessInputMessage::InterceptReply(_)
                        | HarnessInputMessage::GetAgentPromptCreated(_)
                        | HarnessInputMessage::ProviderDebugCapture(_)
                        | HarnessInputMessage::ExtensionDataRequest(_)
                        | HarnessInputMessage::UiDebugEventStatsRequest(_)
                        | HarnessInputMessage::UiDetachRequest(_)
                        | HarnessInputMessage::UiTreeRequest(_)
                )
            } else {
                matches!(
                    (&message, entry.state),
                    (HarnessInputMessage::Hello(_), ExtensionState::Spawning)
                        | (HarnessInputMessage::Disconnect(_), _)
                        | (HarnessInputMessage::Ready(_), ExtensionState::Handshaking)
                        | (
                            HarnessInputMessage::Subscribe(_)
                                | HarnessInputMessage::Intercept(_)
                                | HarnessInputMessage::Emit(_)
                                | HarnessInputMessage::ConfigError(_)
                                | HarnessInputMessage::ExtensionNoticeRequest(_)
                                | HarnessInputMessage::InterceptReply(_)
                                | HarnessInputMessage::GetAgentPromptCreated(_)
                                | HarnessInputMessage::ProviderDebugCapture(_)
                                | HarnessInputMessage::ExtensionDataRequest(_)
                                | HarnessInputMessage::UiDebugEventStatsRequest(_)
                                | HarnessInputMessage::UiDetachRequest(_)
                                | HarnessInputMessage::UiTreeRequest(_),
                            ExtensionState::Handshaking | ExtensionState::Ready,
                        )
                )
            };
            if !legal {
                return self.handle_extension_protocol_failure(
                    source_id,
                    format!(
                        "extension `{}` sent an out-of-order protocol message while {:?}: {:?}",
                        entry.name, entry.state, message
                    ),
                );
            }
        }
        if matches!(
            &message,
            HarnessInputMessage::UiDebugEventStatsRequest(_)
                | HarnessInputMessage::UiDetachRequest(_)
                | HarnessInputMessage::UiTreeRequest(_)
        ) && self.extensions.entries.contains_key(source_id)
        {
            // These requests belong exclusively to attached socket UIs.
            // Configured extensions are silently denied after normal phase
            // validation and metering, before activation staging can turn
            // repeated requests into a quota warning, disconnect, or startup
            // failure.
            return Ok(());
        }
        let activation_pending = self
            .extensions
            .entries
            .get(source_id)
            .is_some_and(|entry| entry.state != ExtensionState::Ready);
        let startup_declaration = matches!(
            &message,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ActionSchemaDeclared(_)
                        | Event::ToolRegistrationDeclared(_)
                        | Event::ToolUnregistrationDeclared(_)
                         | Event::ProviderModelsDeclared(_)
                         | Event::ExtensionContextProviderRegister(_)
                         | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                         | Event::ExtensionSessionContextProviderRegister(_)
                         | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                         | Event::ExtAgentContextPublish(_)
                        | Event::ExtPromptFragmentPublish(_)
                )
        );
        let startup_extension_data_request =
            matches!(&message, HarnessInputMessage::ExtensionDataRequest(_))
                && !self.extensions.ready_received.contains(source_id);
        let operational_message = match &message {
            // Startup declarations are the only emitted events allowed to enter
            // activation staging. Everything else can mutate live state, reply to
            // an earlier request, or acknowledge work, so defer it behind the
            // global activation barrier. Keeping this as a declaration allowlist
            // makes new protocol events safe by default.
            HarnessInputMessage::Emit(_) => !startup_declaration,
            HarnessInputMessage::Hello(_)
            | HarnessInputMessage::Disconnect(_)
            | HarnessInputMessage::Ready(_)
            | HarnessInputMessage::ConfigError(_)
            | HarnessInputMessage::Subscribe(_)
            | HarnessInputMessage::Intercept(_) => false,
            // Initial Configure handlers may need extension-owned storage before
            // they can accept configuration and send Ready (PIM persists its
            // initial state here). Once this peer sends Ready, the same RPC is
            // ordinary operational traffic and retains global barrier ordering.
            HarnessInputMessage::ExtensionDataRequest(_) => !startup_extension_data_request,
            _ => true,
        };
        let retained_message = activation_pending
            && !matches!(
                &message,
                HarnessInputMessage::Hello(_)
                    | HarnessInputMessage::Disconnect(_)
                    | HarnessInputMessage::ConfigError(_)
                    | HarnessInputMessage::Ready(_)
            );
        if retained_message && !self.reserve_extension_activation_message(source_id, &message)? {
            return Ok(());
        }
        let declaration_after_ready =
            startup_declaration && self.extensions.ready_received.contains(source_id);
        if activation_pending && (operational_message || declaration_after_ready) {
            self.defer_extension_activation_message(source_id, message, admission);
            return Ok(());
        }
        match message {
            HarnessInputMessage::Hello(hello) => {
                if let Err(error) = validate_protocol_version(&hello) {
                    return self.handle_extension_protocol_failure(
                        source_id,
                        format!("extension protocol handshake failed: {error}"),
                    );
                }
                if let Some(entry) = self.extensions.entries.get_mut(source_id) {
                    entry.peer_capabilities = hello.capabilities.into_iter().collect();
                }
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.send_lifecycle_configure(source_id);
            }
            HarnessInputMessage::ConfigError(err) => {
                let diagnostic = bounded_extension_config_error(err.message);
                let name = self
                    .extensions
                    .entries
                    .get(source_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| {
                        tau_proto::ExtensionName::parse("extension").expect(
                            "fallback extension name must satisfy the extension identifier grammar",
                        )
                    });
                let optional = self
                    .extensions
                    .entries
                    .get(source_id)
                    .is_some_and(|entry| !entry.require);
                // This is the last line of defense for every extension's typed
                // configuration schema. Do not downgrade, drop, or make this
                // startup-only: invalid extension config must be visible in the
                // UI even when it is reported before any UI client subscribes.
                self.emit_notice(
                    tau_proto::notice_kind::EXTENSION_CONFIG_ERROR,
                    tau_proto::NoticeLevel::Warning,
                    tau_proto::NoticePurpose::Alert,
                    &format!(
                        "extension {name} rejected its config: {}\ncheck \
                         `extensions.{name}.config` and `extensions.{name}.secrets` in harness.yaml; \
                         invalid values are being ignored",
                        diagnostic,
                    ),
                );
                if !self.initial_extension_tool_preflight_complete {
                    if !optional {
                        return Err(HarnessError::Participant(format!(
                            "required extension `{name}` rejected its config: {diagnostic}"
                        )));
                    }
                    tracing::warn!(
                        target: "tau_harness::startup",
                        extension = %name,
                        error = %diagnostic,
                        "optional extension did not initialize: rejected config"
                    );
                    self.disable_optional_extension(
                        source_id,
                        &format!("optional extension {name} did not initialize"),
                    );
                    self.maybe_finish_extension_activation(Some(source_id))?;
                } else {
                    self.handle_disconnect(source_id);
                }
            }
            HarnessInputMessage::ExtensionNoticeRequest(request) => {
                self.handle_extension_notice_request(source_id, request);
            }
            HarnessInputMessage::Subscribe(subscribe) => {
                // Extensions get the same subscribe-time catch-up as UI
                // clients: current-state announcements plus selector-matched
                // durable facts as replay-marked frames. Side-effecting
                // extensions must skip replay frames instead of being
                // protected by withheld delivery.
                self.complete_subscription(
                    source_id,
                    subscribe.historical_selectors,
                    subscribe.live_selectors,
                )
                .map_err(HarnessError::Route)?;
            }
            HarnessInputMessage::Intercept(intercept) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_intercept(source_id, intercept);
                } else {
                    self.register_extension_interceptor(source_id, intercept);
                }
            }
            HarnessInputMessage::Ready(_ready) => {
                self.extensions.startup_deadlines.remove(source_id);
                self.extensions.expired_startup_connects.remove(source_id);
                self.extensions.ready_received.insert(source_id.clone());
                self.maybe_finish_extension_activation(Some(source_id))?;
                self.drain_pending_tool_invocations()?;
                self.try_advance_queue();
            }
            HarnessInputMessage::Emit(emit) => {
                // Governing contract:
                // `specs/SPEC-peer-event-publication.md`.
                // `Emit` is a private protocol submission request, not a committed
                // event fact. Keep this arm a generic
                // admission/interception/commit chokepoint: never add
                // concrete-event semantics here. Move each family to
                // committed-event processing or a dedicated protocol message
                // instead.
                let (event, persist) = emit.into_parts();
                self.handle_extension_event_inner_with_admission(
                    source_id,
                    event,
                    Some(persist),
                    admission,
                )?;
            }
            HarnessInputMessage::InterceptReply(reply) => {
                self.handle_intercept_reply(source_id, reply)?;
            }
            HarnessInputMessage::GetAgentPromptCreated(request) => {
                self.send_agent_prompt_created_result(source_id, request);
            }
            HarnessInputMessage::ExtensionDataRequest(request) => {
                self.handle_extension_data_request(source_id, request, admission);
            }
            HarnessInputMessage::ProviderDebugCapture(capture) => {
                self.handle_provider_debug_capture(source_id, capture);
            }
            // Messages sent by clients only — extensions shouldn't round-trip
            // these. Ignore silently.
            HarnessInputMessage::Disconnect(_)
            | HarnessInputMessage::GetRenderedSystemPrompt(_)
            | HarnessInputMessage::GetRenderedPrompt(_)
            | HarnessInputMessage::GetRenderedToolDefinitions(_)
            | HarnessInputMessage::GetCurrentSession(_)
            | HarnessInputMessage::GetSessionAgentList(_)
            | HarnessInputMessage::UiDebugEventStatsRequest(_)
            | HarnessInputMessage::UiDetachRequest(_)
            | HarnessInputMessage::UiTreeRequest(_)
            | HarnessInputMessage::ExternalAgentMessage(_)
            | HarnessInputMessage::ExternalAgentMessageAuth(_)
            | HarnessInputMessage::PeerSessionProbe(_) => {}
        }
        Ok(())
    }

    /// Attribute one opaque capture from a cooperative configured Provider and
    /// queue it on the harness-owned filesystem path.
    pub(super) fn handle_provider_debug_capture(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        capture: tau_proto::ProviderDebugCapture,
    ) {
        let Some((session_dir, provider_instance)) =
            self.provider_debug_capture_target(source_id, &capture)
        else {
            return;
        };
        crate::provider_capture_writer::enqueue(session_dir, provider_instance, capture);
    }

    /// Resolve structured Provider attribution without consulting the current
    /// prompt route, which may already have completed.
    pub(super) fn provider_debug_capture_target(
        &self,
        source_id: &tau_proto::ConnectionId,
        capture: &tau_proto::ProviderDebugCapture,
    ) -> Option<(PathBuf, tau_proto::ExtensionName)> {
        let provider_instance = self.extensions.entries.get(source_id).and_then(|entry| {
            (entry.kind == ClientKind::Provider
                && entry.state == ExtensionState::Ready
                && entry.connection_id == *source_id)
                .then(|| entry.name.clone())
        })?;
        if self.storage_mode.is_ephemeral() {
            return None;
        }
        let session_dir = self.sessions_dir().join(capture.session_id.as_str());
        if !session_dir.is_dir() {
            return None;
        }
        Some((session_dir, provider_instance))
    }
}
