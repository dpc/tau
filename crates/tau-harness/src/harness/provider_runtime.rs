//! Process-global provider model routing, selection, and quota snapshots.
//!
//! Provider declarations remain harness-wide rather than session-owned. This
//! module preserves the extension boundary and event sequencing described by
//! [ARCH-tau-harness](../../specs/ARCH-tau-harness.md).

use super::*;

/// Harness-owned validated quota state for one provider extension route.
#[derive(Clone)]
pub(crate) struct CurrentProviderQuota {
    /// Extension connection that established this state.
    pub(super) source_id: tau_proto::ConnectionId,
    /// Latest full provider-neutral snapshot.
    pub(crate) snapshot: tau_proto::HarnessProviderQuotaChanged,
}
#[derive(Clone)]
pub(super) struct ProviderQuotaTombstone {
    source_id: tau_proto::ConnectionId,
    profile_epoch: tau_proto::ProviderQuotaEpoch,
    sequence: tau_proto::ProviderQuotaSequence,
}

impl Harness {
    /// Validate model entries independently, publish one diagnostic per
    /// rejected entry, and narrow accepted no-tool routes to non-parallel
    /// capability.
    pub(super) fn validate_provider_models_declaration(
        &mut self,
        publisher_extension_id: &tau_proto::ExtensionName,
        declaration: tau_proto::ProviderModelsDeclared,
    ) -> tau_proto::ProviderModelsDeclared {
        let mut accepted = Vec::with_capacity(declaration.models.len());
        for mut model in declaration.models {
            let mut issues = Vec::new();
            if model.context_window == tau_proto::TokenCount::ZERO {
                issues.push(tau_proto::ProviderModelDeclarationIssue::ContextWindowZero);
            }
            let has_standalone_metadata = model.standalone_compaction_threshold.is_some()
                || model.standalone_compaction_prefix_budget.is_some();
            if has_standalone_metadata && !model.supports_explicit_standalone_compaction() {
                issues
                    .push(tau_proto::ProviderModelDeclarationIssue::StandaloneMetadataUnsupported);
            }
            if model.standalone_compaction_threshold == Some(tau_proto::TokenCount::ZERO) {
                issues.push(
                    tau_proto::ProviderModelDeclarationIssue::StandaloneCompactionThresholdZero,
                );
            }
            if model
                .standalone_compaction_threshold
                .is_some_and(|threshold| threshold > model.context_window)
            {
                issues.push(
                    tau_proto::ProviderModelDeclarationIssue::StandaloneCompactionThresholdExceedsContextWindow,
                );
            }
            if model.standalone_compaction_prefix_budget == Some(tau_proto::ByteCount::ZERO) {
                issues.push(
                    tau_proto::ProviderModelDeclarationIssue::StandaloneCompactionPrefixBudgetZero,
                );
            }
            if model.supported_tool_types.is_empty() {
                model.supports_parallel_tool_calls = false;
            }
            if issues.is_empty() {
                accepted.push(model);
            } else {
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ProviderModelDeclarationDiagnostic(
                        tau_proto::ProviderModelDeclarationDiagnostic {
                            publisher_extension_id: publisher_extension_id.clone(),
                            model: model.id,
                            issues,
                        },
                    ),
                );
            }
        }
        tau_proto::ProviderModelsDeclared { models: accepted }
    }

    pub(super) fn publish_provider_models_update(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        publisher_extension_id: tau_proto::ExtensionName,
        declaration: tau_proto::ProviderModelsDeclared,
    ) {
        let declaration =
            self.validate_provider_models_declaration(&publisher_extension_id, declaration);
        let update = tau_proto::ProviderModelsUpdated {
            publisher_extension_id,
            models: declaration.models,
        };
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ProviderModelsUpdated(update.clone()),
        );
        self.apply_provider_models_snapshot(source_id, update.models);
    }
    /// Applies one authoritative snapshot from a ready provider and reconciles
    /// restored work against models explicitly removed by that provider.
    pub(super) fn apply_provider_models_snapshot(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        models: Vec<ProviderModelInfo>,
    ) {
        let previous_model_info = self
            .provider_runtime
            .models_by_extension
            .get(source_id)
            .map(|models| {
                models
                    .iter()
                    .map(|model| (model.id.clone(), model.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let previous_models = previous_model_info.keys().cloned().collect::<HashSet<_>>();
        let updated_model_info = models
            .iter()
            .map(|model| (model.id.clone(), model.clone()))
            .collect::<HashMap<_, _>>();
        let updated_models = models
            .iter()
            .map(|model| model.id.clone())
            .collect::<HashSet<_>>();
        let removed_models = previous_models
            .difference(&updated_models)
            .cloned()
            .collect::<HashSet<_>>();
        let changed_models = updated_model_info
            .iter()
            .filter(|(model, info)| {
                previous_model_info
                    .get(*model)
                    .is_some_and(|previous| previous != *info)
            })
            .map(|(model, _)| model.clone())
            .chain(removed_models.iter().cloned())
            .collect::<HashSet<_>>();
        self.set_provider_models(source_id, models);
        self.clear_changed_quota_bindings(source_id, &changed_models);
        self.clear_unowned_provider_quota();
        self.reconcile_pending_context_recoveries(false);
        self.resume_restored_compaction_checkpoints(
            RestoredCheckpointAuthority::ExplicitlyRemoved(&removed_models),
        );
    }
    fn clear_unowned_provider_quota(&mut self) {
        let providers = self
            .provider_runtime
            .quota
            .iter()
            .filter(|(provider, current)| {
                !self.source_owns_quota_provider(&current.source_id, provider)
            })
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>();
        for provider in providers {
            self.remove_provider_quota(&provider);
        }
    }
    fn clear_changed_quota_bindings(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        changed_models: &HashSet<ModelId>,
    ) {
        if changed_models.is_empty() {
            return;
        }
        let providers = self
            .provider_runtime
            .quota
            .iter()
            .filter(|(_, current)| current.source_id == *source_id)
            .filter(|(_, current)| {
                current
                    .snapshot
                    .route_bindings
                    .iter()
                    .any(|binding| changed_models.contains(&binding.model))
            })
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>();
        for provider in providers {
            let Some(current) = self.provider_runtime.quota.get_mut(&provider) else {
                continue;
            };
            current
                .snapshot
                .route_bindings
                .retain(|binding| !changed_models.contains(&binding.model));
            let changed = current.snapshot.clone();
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::HarnessProviderQuotaChanged(changed),
            );
        }
    }
    fn refresh_provider_model_info(&mut self) {
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::ProviderRotated);
        let mut provider_model_info = HashMap::new();
        let mut provider_model_routes = HashMap::new();
        let mut duplicate_model_ids = HashSet::new();
        let mut source_ids: Vec<_> = self.provider_runtime.models_by_extension.keys().collect();
        source_ids.sort();
        for source_id in source_ids {
            let Some(models) = self.provider_runtime.models_by_extension.get(source_id) else {
                continue;
            };
            let connection_id = source_id.clone();
            for model in models {
                if provider_model_info
                    .insert(model.id.clone(), model.clone())
                    .is_some()
                {
                    duplicate_model_ids.insert(model.id.clone());
                }
                provider_model_routes.insert(model.id.clone(), connection_id.clone());
            }
        }
        self.provider_runtime.model_info = provider_model_info;
        self.provider_runtime.model_routes = provider_model_routes;
        self.warn_on_duplicate_provider_models(duplicate_model_ids);
    }
    /// Warns about ambiguous provider-qualified ids without changing the
    /// existing sorted-source, last-advertisement-wins registry behavior.
    fn warn_on_duplicate_provider_models(&mut self, duplicate_model_ids: HashSet<ModelId>) {
        const DISPLAY_LIMIT: usize = 8;

        if duplicate_model_ids.is_empty() {
            return;
        }
        let total = duplicate_model_ids.len();
        let mut duplicate_model_ids = duplicate_model_ids.into_iter().collect::<Vec<_>>();
        duplicate_model_ids.sort();
        duplicate_model_ids.truncate(DISPLAY_LIMIT);
        let displayed = duplicate_model_ids
            .iter()
            .map(Self::bounded_model_id_label)
            .collect::<Vec<_>>()
            .join(", ");
        let omitted = total.saturating_sub(duplicate_model_ids.len());
        let suffix = if omitted == 0 {
            String::new()
        } else {
            format!(" (and {omitted} more)")
        };
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Diagnostic,
            &format!(
                "duplicate provider-qualified model ids advertised: {displayed}{suffix}; \
                 preserving sorted-source last-wins routing"
            ),
        );
    }
    /// Produces a single-line, UTF-8-safe diagnostic label with a fixed byte
    /// cap.
    fn bounded_model_id_label(model_id: &ModelId) -> String {
        const BYTE_LIMIT: usize = 96;
        const ELLIPSIS: &str = "…";

        let raw = model_id.to_string();
        let mut label = String::with_capacity(BYTE_LIMIT);
        let mut truncated = false;
        for character in raw.chars() {
            let unsafe_for_display = character.is_control()
                || matches!(
                    character,
                    '\u{00ad}'
                        | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{fdd0}'..='\u{fdef}'
                        | '\u{feff}'
                )
                || (character as u32 & 0xffff == 0xfffe)
                || (character as u32 & 0xffff == 0xffff);
            let character = if unsafe_for_display {
                '\u{fffd}'
            } else {
                character
            };
            if label.len() + character.len_utf8() > BYTE_LIMIT - ELLIPSIS.len() {
                truncated = true;
                break;
            }
            label.push(character);
        }
        if truncated {
            label.push_str(ELLIPSIS);
        }
        label
    }
    fn refresh_available_models(&mut self) {
        self.refresh_provider_model_info();
        let mut models: Vec<ModelId> = self.provider_runtime.model_info.keys().cloned().collect();
        models.sort();
        self.provider_runtime.available_models = models;
    }
    pub(super) fn role_after_update(
        &mut self,
        role_name: &str,
        action: tau_proto::UiRoleUpdateAction,
    ) -> Option<tau_config::settings::AgentRole> {
        let mut next_role = self
            .config
            .available_roles
            .get(role_name)
            .cloned()
            .unwrap_or_default();
        let effective_params = model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            role_name,
        )
        .map(|model| self.params_for_role_model(role_name, &model))
        .unwrap_or_default();

        match action {
            tau_proto::UiRoleUpdateAction::Delete => unreachable!("handled by caller"),
            tau_proto::UiRoleUpdateAction::SetModel { model } => {
                next_role.model = model;
            }
            tau_proto::UiRoleUpdateAction::SetEffort { effort } => {
                next_role.effort = effort;
            }
            tau_proto::UiRoleUpdateAction::AdjustEffort { adjustment } => {
                next_role.effort = Some(effective_params.effort.adjust(adjustment));
            }
            tau_proto::UiRoleUpdateAction::SetVerbosity { verbosity } => {
                next_role.verbosity = verbosity;
            }
            tau_proto::UiRoleUpdateAction::AdjustVerbosity { adjustment } => {
                next_role.verbosity = Some(effective_params.verbosity.adjust(adjustment));
            }
            tau_proto::UiRoleUpdateAction::SetThinkingSummary { thinking_summary } => {
                next_role.thinking_summary = thinking_summary;
            }
            tau_proto::UiRoleUpdateAction::AdjustThinkingSummary { adjustment } => {
                next_role.thinking_summary =
                    Some(effective_params.thinking_summary.adjust(adjustment));
            }
            tau_proto::UiRoleUpdateAction::SetServiceTier { service_tier } => {
                next_role.service_tier = service_tier;
            }
            tau_proto::UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold,
            } => {
                let inference = match compaction_threshold {
                    Some(threshold) => {
                        path_tau_config_settings::RoleCompaction::Threshold(threshold.get())
                    }
                    None => path_tau_config_settings::RoleCompaction::ProviderDefault,
                };
                next_role.compaction = Some(inference);
                next_role.inference_compaction = Some(inference);
                let threshold = match inference {
                    path_tau_config_settings::RoleCompaction::Threshold(tokens) => {
                        path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens)
                    }
                    path_tau_config_settings::RoleCompaction::ProviderDefault => {
                        path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault
                    }
                    path_tau_config_settings::RoleCompaction::Disabled => {
                        unreachable!("legacy UI command never selects disabled")
                    }
                };
                next_role
                    .compactions
                    .entry("default".to_owned())
                    .and_modify(|policy| {
                        policy.threshold = threshold;
                        policy.enable = true;
                    })
                    .or_insert(path_tau_config_settings::CompactionPolicy {
                        threshold,
                        enable: true,
                        when: Default::default(),
                    });
            }
            tau_proto::UiRoleUpdateAction::SetTools { tools } => {
                next_role.tools = tools;
            }
            tau_proto::UiRoleUpdateAction::SetEnableToolGroups { enable_tool_groups } => {
                next_role.enable_tool_groups = enable_tool_groups;
            }
            tau_proto::UiRoleUpdateAction::SetDisableToolGroups {
                disable_tool_groups,
            } => {
                next_role.disable_tool_groups = disable_tool_groups;
            }
            tau_proto::UiRoleUpdateAction::SetEnableTools { enable_tools } => {
                next_role.enable_tools = enable_tools;
            }
            tau_proto::UiRoleUpdateAction::SetDisableTools { disable_tools } => {
                next_role.disable_tools = disable_tools;
            }
        }

        Some(next_role)
    }
    pub(super) fn reconcile_selected_model_with_available(&mut self) {
        let previous_model = self.config.selected_model.clone();
        self.config.selected_model = select_model_for_role(
            &self.provider_runtime.model_info,
            &self.config.available_roles,
            &self.config.selected_role,
        );
        if previous_model != self.config.selected_model {
            self.session_runtime
                .current_session_state
                .context_input_tokens = None;
            self.session_runtime
                .current_session_state
                .context_cached_tokens = None;
            self.session_runtime
                .current_session_state
                .context_percent_used = None;
        }
    }
    pub(super) fn refresh_provider_models_and_publish_state(&mut self) {
        let had_provider_models = !self.provider_runtime.model_info.is_empty();
        let had_routable_model = self
            .config
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_runtime.model_routes.contains_key(model));
        self.refresh_available_models();
        self.reconcile_selected_model_with_available();
        self.reconcile_agent_context_usage_models();
        self.publish_available_model_state();
        let has_provider_models = !self.provider_runtime.model_info.is_empty();
        let has_routable_model = self
            .config
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_runtime.model_routes.contains_key(model));
        if self.session_runtime.turn_state.is_idle()
            && ((!had_routable_model && has_routable_model)
                || (!had_provider_models && has_provider_models))
        {
            self.try_advance_queue();
        }
    }
    pub(super) fn reconcile_agent_context_usage_models(&mut self) {
        let resolutions: Vec<_> = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .filter_map(|(cid, conv)| {
                conv.execution
                    .context_usage_model
                    .as_ref()
                    .map(|usage_model| {
                        (
                            cid.clone(),
                            usage_model.clone(),
                            self.model_for_agent_role(conv),
                        )
                    })
            })
            .collect();
        for (cid, usage_model, current_model) in resolutions {
            if current_model.is_none()
                && !self.provider_runtime.model_info.contains_key(&usage_model)
            {
                // Provider discovery is staggered. Absence is not yet evidence
                // that another model owns this agent, so keep the qualified
                // baseline until its provider either appears or resolution
                // becomes a confirmed mismatch.
                continue;
            }
            if current_model.as_ref() != Some(&usage_model) {
                self.clear_agent_context_usage(&cid);
                continue;
            }
            let context_window =
                context_window_for_model(&self.provider_runtime.model_info, &usage_model);
            if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                conv.execution.context_percent_used =
                    match (context_window, conv.execution.context_input_tokens) {
                        (Some(window), Some(tokens)) => Some(context_percent_used(tokens, window)),
                        _ => None,
                    };
            }
        }
    }
    fn publish_available_model_state(&mut self) {
        self.publish_event(
            None,
            Event::HarnessModelsAvailable(tau_proto::HarnessModelsAvailable {
                models: self.provider_runtime.available_models.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_runtime.model_info,
                    &self.config.available_roles,
                    &self.provider_runtime.available_models,
                ),
                groups: self.current_role_groups(),
                custom_prompts: self.config.custom_prompts.clone(),
            }),
        );
        self.publish_delegate_roles_context();
        self.publish_current_model_state();
    }
    pub(super) fn current_role_groups(&self) -> Vec<tau_proto::HarnessRoleGroup> {
        let mut grouped = HashSet::new();
        let mut groups = Vec::new();
        for group in &self.config.available_role_groups {
            let mut roles: Vec<_> = group
                .roles
                .iter()
                .filter(|role| self.config.available_roles.contains_key(*role))
                .inspect(|role| {
                    grouped.insert((*role).clone());
                })
                .cloned()
                .collect();
            crate::model::sort_role_group_roles(&self.config.available_roles, &mut roles);
            if !roles.is_empty() {
                groups.push(tau_proto::HarnessRoleGroup {
                    name: group.name.clone(),
                    roles,
                });
            }
        }
        let mut ungrouped: Vec<_> = self
            .config
            .available_roles
            .keys()
            .filter(|role| !grouped.contains(*role))
            .cloned()
            .collect();
        ungrouped.sort();
        groups.extend(
            ungrouped
                .into_iter()
                .map(|role| tau_proto::HarnessRoleGroup {
                    name: role.clone(),
                    roles: vec![role],
                }),
        );
        groups
    }
    pub(super) fn publish_current_model_state(&mut self) {
        let selected_model = self.config.selected_model.clone();
        let (effort_levels, verbosity_levels, thinking_levels) =
            if let Some(model) = selected_model.as_ref() {
                (
                    efforts_for_model(&self.provider_runtime.model_info, model),
                    verbosities_for_model(&self.provider_runtime.model_info, model),
                    thinking_summaries_for_model(&self.provider_runtime.model_info, model),
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
        let context_window = selected_model
            .as_ref()
            .and_then(|model| context_window_for_model(&self.provider_runtime.model_info, model));
        self.session_runtime
            .current_session_state
            .context_percent_used = match (
            context_window,
            self.session_runtime
                .current_session_state
                .context_input_tokens,
        ) {
            (Some(context_window), Some(input_tokens)) => {
                Some(context_percent_used(input_tokens, context_window))
            }
            _ => None,
        };
        self.publish_event(
            None,
            Event::HarnessRoleSelected(HarnessRoleSelected {
                baseline_params: selected_model.as_ref().map(|model| {
                    baseline_params_for_selection(
                        &self.config.accepted_harness_settings,
                        &self.provider_runtime.model_info,
                        &self.config.selected_role,
                        model,
                    )
                }),
                model_params: selected_model
                    .as_ref()
                    .map(|model| self.params_for_role_model(&self.config.selected_role, model))
                    .unwrap_or_default(),
                model: selected_model,
                context_window: context_window.map(tau_proto::TokenCount::get),
                role: self.config.selected_role.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                input_tokens: self
                    .session_runtime
                    .current_session_state
                    .context_input_tokens
                    .map(tau_proto::TokenCount::get),
                cached_tokens: self
                    .session_runtime
                    .current_session_state
                    .context_cached_tokens
                    .map(tau_proto::TokenCount::get),
                percent_used: self
                    .session_runtime
                    .current_session_state
                    .context_percent_used,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                levels: effort_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessVerbositiesAvailable(tau_proto::HarnessVerbositiesAvailable {
                levels: verbosity_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessThinkingSummariesAvailable(
                tau_proto::HarnessThinkingSummariesAvailable {
                    levels: thinking_levels,
                },
            ),
        );
    }
    pub(super) fn set_provider_models(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        models: Vec<ProviderModelInfo>,
    ) {
        self.provider_runtime
            .models_by_extension
            .insert(source_id.to_owned(), models);
        self.refresh_provider_models_and_publish_state();
    }
    fn source_owns_quota_provider(
        &self,
        source_id: &tau_proto::ConnectionId,
        provider: &tau_proto::ProviderName,
    ) -> bool {
        let owners = self
            .provider_runtime
            .model_routes
            .iter()
            .filter(|(model, _)| &model.provider == provider)
            .map(|(_, route)| route)
            .collect::<HashSet<_>>();
        owners.len() == 1 && owners.contains(source_id)
    }
    fn quota_event_is_valid(
        &self,
        source_id: &tau_proto::ConnectionId,
        provider: &tau_proto::ProviderName,
        windows: &[tau_proto::ProviderQuotaWindow],
        bindings: &[tau_proto::ProviderQuotaRouteBinding],
    ) -> bool {
        self.source_owns_quota_provider(source_id, provider)
            && bindings.iter().all(|binding| {
                self.provider_runtime
                    .model_routes
                    .get(&binding.model)
                    .is_some_and(|route| route == source_id)
            })
            && tau_proto::validate_provider_quota_state(provider, windows, bindings).is_ok()
    }
    pub(super) fn handle_provider_quota_replace(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        replace: tau_proto::ProviderQuotaReplace,
    ) {
        if !self.quota_event_is_valid(
            source_id,
            &replace.provider,
            &replace.windows,
            &replace.route_bindings,
        ) {
            tracing::warn!(
                target: "tau_harness",
                source_id = %source_id,
                provider = %replace.provider,
                "discarding invalid or unowned provider quota replacement"
            );
            return;
        }
        let current = self.provider_runtime.quota.get(&replace.provider);
        let accepted = if replace.establishes_new_epoch {
            !self
                .provider_runtime
                .quota_retired_epochs
                .get(&replace.provider)
                .is_some_and(|epochs| epochs.contains(&replace.profile_epoch))
                && current.is_none_or(|current| {
                    current.source_id != *source_id
                        || current.snapshot.profile_epoch != replace.profile_epoch
                })
        } else {
            current.is_some_and(|current| {
                current.source_id == *source_id
                    && current.snapshot.profile_epoch == replace.profile_epoch
                    && replace.sequence > current.snapshot.sequence
            }) || (current.is_none()
                && self
                    .provider_runtime
                    .quota_tombstones
                    .get(&replace.provider)
                    .is_some_and(|tombstone| {
                        tombstone.source_id == *source_id
                            && tombstone.profile_epoch == replace.profile_epoch
                            && replace.sequence > tombstone.sequence
                    }))
                || (current.is_none()
                    && !self
                        .provider_runtime
                        .quota_retired_epochs
                        .get(&replace.provider)
                        .is_some_and(|epochs| epochs.contains(&replace.profile_epoch)))
        };
        if !accepted {
            return;
        }
        if let Some(previous) = current.cloned()
            && previous.snapshot.profile_epoch != replace.profile_epoch
        {
            self.retire_provider_quota_epoch(&replace.provider, previous.snapshot.profile_epoch);
        }
        let changed = tau_proto::HarnessProviderQuotaChanged {
            provider: replace.provider.clone(),
            profile_epoch: replace.profile_epoch,
            sequence: replace.sequence,
            windows: replace.windows,
            route_bindings: replace.route_bindings,
        };
        self.provider_runtime
            .quota_tombstones
            .remove(&replace.provider);
        self.provider_runtime
            .quota_capabilities
            .remove(&replace.provider);
        self.provider_runtime.quota.insert(
            replace.provider,
            CurrentProviderQuota {
                source_id: source_id.clone(),
                snapshot: changed.clone(),
            },
        );
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessProviderQuotaChanged(changed),
        );
    }
    pub(super) fn handle_provider_quota_patch(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        patch: tau_proto::ProviderQuotaPatch,
    ) {
        let patch_window_keys = patch
            .windows
            .iter()
            .map(|window| &window.key)
            .collect::<HashSet<_>>();
        let removed_keys = patch.removed_window_keys.iter().collect::<HashSet<_>>();
        let binding_models = patch
            .route_bindings
            .iter()
            .map(|binding| &binding.model)
            .collect::<HashSet<_>>();
        if patch.windows.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
            || patch.removed_window_keys.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
            || patch.route_bindings.len() > tau_proto::MAX_PROVIDER_QUOTA_BINDINGS
            || patch_window_keys.len() != patch.windows.len()
            || removed_keys.len() != patch.removed_window_keys.len()
            || binding_models.len() != patch.route_bindings.len()
            || patch_window_keys
                .iter()
                .any(|key| removed_keys.contains(key))
            || !self.source_owns_quota_provider(source_id, &patch.provider)
        {
            return;
        }
        let Some(current) = self.provider_runtime.quota.get(&patch.provider) else {
            return;
        };
        if current.source_id != *source_id
            || current.snapshot.profile_epoch != patch.profile_epoch
            || patch.sequence <= current.snapshot.sequence
        {
            return;
        }
        let mut windows = current
            .snapshot
            .windows
            .iter()
            .cloned()
            .map(|window| (window.key.clone(), window))
            .collect::<std::collections::BTreeMap<_, _>>();
        for key in patch.removed_window_keys {
            windows.remove(&key);
        }
        for window in patch.windows {
            windows.insert(window.key.clone(), window);
        }
        let mut bindings = current
            .snapshot
            .route_bindings
            .iter()
            .cloned()
            .map(|binding| (binding.model.clone(), binding))
            .collect::<std::collections::BTreeMap<_, _>>();
        for binding in patch.route_bindings {
            bindings.insert(binding.model.clone(), binding);
        }
        let changed = tau_proto::HarnessProviderQuotaChanged {
            provider: patch.provider.clone(),
            profile_epoch: patch.profile_epoch,
            sequence: patch.sequence,
            windows: windows.into_values().collect(),
            route_bindings: bindings.into_values().collect(),
        };
        if tau_proto::validate_provider_quota_state(
            &changed.provider,
            &changed.windows,
            &changed.route_bindings,
        )
        .is_err()
        {
            return;
        }
        self.provider_runtime.quota.insert(
            patch.provider,
            CurrentProviderQuota {
                source_id: source_id.clone(),
                snapshot: changed.clone(),
            },
        );
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessProviderQuotaChanged(changed),
        );
    }
    pub(super) fn handle_provider_quota_clear(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        clear: tau_proto::ProviderQuotaClear,
    ) {
        let matches = self
            .provider_runtime
            .quota
            .get(&clear.provider)
            .is_some_and(|current| {
                current.source_id == *source_id
                    && current.snapshot.profile_epoch == clear.profile_epoch
                    && clear.sequence > current.snapshot.sequence
            });
        if matches {
            self.remove_provider_quota_at(&clear.provider, clear.sequence, false);
            return;
        }
        let tombstone_matches = self
            .provider_runtime
            .quota_tombstones
            .get(&clear.provider)
            .is_some_and(|tombstone| {
                tombstone.source_id == *source_id
                    && tombstone.profile_epoch == clear.profile_epoch
                    && clear.sequence > tombstone.sequence
            });
        if tombstone_matches {
            self.provider_runtime
                .quota_tombstones
                .remove(&clear.provider);
            self.retire_provider_quota_epoch(&clear.provider, clear.profile_epoch);
        }
    }
    pub(super) fn remove_provider_quota(&mut self, provider: &tau_proto::ProviderName) {
        let sequence = self
            .provider_runtime
            .quota
            .get(provider)
            .map_or_else(tau_proto::ProviderQuotaSequence::default, |current| {
                current.snapshot.sequence
            });
        self.remove_provider_quota_at(provider, sequence, true);
    }
    fn remove_provider_quota_at(
        &mut self,
        provider: &tau_proto::ProviderName,
        sequence: tau_proto::ProviderQuotaSequence,
        allow_recovery: bool,
    ) {
        let Some(current) = self.provider_runtime.quota.remove(provider) else {
            return;
        };
        self.retire_provider_quota_epoch(provider, current.snapshot.profile_epoch.clone());
        if allow_recovery {
            self.provider_runtime.quota_tombstones.insert(
                provider.clone(),
                ProviderQuotaTombstone {
                    source_id: current.source_id,
                    profile_epoch: current.snapshot.profile_epoch.clone(),
                    sequence,
                },
            );
        } else {
            self.provider_runtime.quota_tombstones.remove(provider);
        }
        let changed = tau_proto::HarnessProviderQuotaChanged {
            provider: provider.clone(),
            profile_epoch: current.snapshot.profile_epoch,
            sequence,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        };
        self.provider_runtime
            .quota_capabilities
            .insert(provider.clone(), changed.clone());
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessProviderQuotaChanged(changed),
        );
    }
    fn retire_provider_quota_epoch(
        &mut self,
        provider: &tau_proto::ProviderName,
        epoch: tau_proto::ProviderQuotaEpoch,
    ) {
        const MAX_RETIRED_EPOCHS: usize = 8;
        let epochs = self
            .provider_runtime
            .quota_retired_epochs
            .entry(provider.clone())
            .or_default();
        if !epochs.contains(&epoch) {
            epochs.push_back(epoch);
        }
        while epochs.len() > MAX_RETIRED_EPOCHS {
            epochs.pop_front();
        }
    }
}
